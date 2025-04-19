use actix_files as fs;
use actix_web::http::header::{
    ContentDisposition, DispositionType, CACHE_CONTROL, IF_MODIFIED_SINCE, LAST_MODIFIED,
};
use actix_web::http::StatusCode;
use actix_web::{
    get, middleware::Logger, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
    ResponseError,
};
use clap::Parser;
use httpdate::HttpDate;
use image::error::ImageError;
use image::io::Reader as ImageReader;
use image::{DynamicImage, ImageOutputFormat};
use psd::Psd;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
enum Size {
    Small,
    Medium,
    Large,
}

impl Size {
    fn from_str(s: &str) -> Self {
        match s {
            "small" => Size::Small,
            "large" => Size::Large,
            _ => Size::Medium,
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        match self {
            Size::Small => (120, 120),
            Size::Medium => (300, 300),
            Size::Large => (600, 600),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound(),

    #[error("malformed key {0}")]
    InvalidKey(String),

    #[error("Failed to decode: err={0}")]
    FailedToDecode(ImageError),

    #[error("Failed to encode: err={0}")]
    FailedToEncode(ImageError),
}

impl ResponseError for ApiError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        match self {
            ApiError::NotFound() => StatusCode::NOT_FOUND,
            ApiError::InvalidKey(_) => StatusCode::NOT_FOUND,
            ApiError::FailedToDecode(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::FailedToEncode(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).finish()
        // let response_body = match self {
        //     AppError::NotFound() => {
        //         serde_json::json!(errors)
        //     }
        // };
        // HttpResponseBuilder::new(self.status_code()).json(response_body)
    }
}

fn path_from_key(base_path: &Path, key: &str) -> Result<std::path::PathBuf, ApiError> {
    let (hkey, ext) = key.split_once('.').unwrap_or((key, ""));
    if hkey.len() != 32 {
        log::debug!("Malformed hash key {}", hkey);
        return Err(ApiError::InvalidKey(key.to_string()));
    }
    if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        log::debug!("Malformed ext: key={}, ext={}", key, ext);
        return Err(ApiError::InvalidKey(key.to_string()));
    }

    if !hkey.chars().all(|c| c.is_ascii_hexdigit()) {
        log::debug!("Malformed hash key {}", hkey);
        return Err(ApiError::InvalidKey(key.to_string()));
    }

    let prefix = hkey.get(0..2).unwrap();
    let mut path = PathBuf::from(base_path);
    path.push(prefix);
    path.push(key);

    Ok(path)
}

fn is_not_modified(req: &HttpRequest, modified_time: SystemTime) -> bool {
    if let Some(ims) = req.headers().get(IF_MODIFIED_SINCE) {
        if let Ok(ims_str) = ims.to_str() {
            if let Ok(ims_time) = httpdate::parse_http_date(ims_str) {
                return modified_time <= ims_time;
            }
        }
    }
    return false;
}

#[get("/raw/{tail:.*}")]
async fn original(
    _req: HttpRequest,
    path: web::Path<String>,
    base_path: web::Data<std::path::PathBuf>,
) -> Result<fs::NamedFile, Error> {
    let canonical_path = path_from_key(base_path.get_ref(), &path.into_inner())?;

    let named_file = fs::NamedFile::open(canonical_path)?;
    Ok(named_file
        .use_last_modified(true)
        .set_content_disposition(ContentDisposition {
            disposition: DispositionType::Attachment,
            parameters: vec![],
        }))
}

#[get("/media/{tail:.*}")]
async fn media(
    req: HttpRequest,
    path: web::Path<String>,
    base_path: web::Data<std::path::PathBuf>,
) -> impl Responder {
    let canonical_path = match path_from_key(base_path.get_ref(), &path.into_inner()) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("Mulformed path: err={}", err);
            return HttpResponse::NotFound().body("File not found");
        }
    };

    // ファイルの最終更新日時
    let metadata = match std::fs::metadata(&canonical_path) {
        Ok(meta) => meta,
        Err(_) => return HttpResponse::InternalServerError().body("Failed to read metadata"),
    };

    let modified_time = metadata.modified().unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return HttpResponse::NotModified().finish();
    }

    let img = match load_image(&canonical_path) {
        Ok(img) => img,
        Err(err) => {
            log::warn!("Failed to decode image: {}", err);
            return HttpResponse::InternalServerError().body("Failed to decode image");
        }
    };

    let mut buffer = Cursor::new(Vec::new());
    if let Err(_) = img.write_to(&mut buffer, ImageOutputFormat::WebP) {
        return HttpResponse::InternalServerError().body("Failed to encode image");
    }

    return HttpResponse::Ok()
        .content_type("image/webp")
        .insert_header((CACHE_CONTROL, "public, max-age=2592000"))
        .insert_header((LAST_MODIFIED, HttpDate::from(modified_time).to_string()))
        .body(buffer.into_inner());
}

#[get("/thumbnail/{tail:.*}")]
async fn thumbnail(
    req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    base_path: web::Data<std::path::PathBuf>,
) -> impl Responder {
    let size = query
        .get("size")
        .map(|s| Size::from_str(s))
        .unwrap_or(Size::Medium);
    let canonical_path = match path_from_key(base_path.get_ref(), &path.into_inner()) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("Mulformed path: err={}", err);
            return HttpResponse::NotFound().body("File not found");
        }
    };

    // ファイルの最終更新日時
    let metadata = match std::fs::metadata(&canonical_path) {
        Ok(meta) => meta,
        Err(_) => return HttpResponse::InternalServerError().body("Failed to read metadata"),
    };

    let modified_time = metadata.modified().unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return HttpResponse::NotModified().finish();
    }

    let img = match load_image(&canonical_path) {
        Ok(img) => img,
        Err(err) => {
            log::warn!("Failed to decode image: {}", err);
            return HttpResponse::InternalServerError().body("Failed to decode image");
        }
    };

    let (w, h) = size.dimensions();
    let resized = img.thumbnail(w, h);

    let mut buffer = Cursor::new(Vec::new());
    if let Err(_) = resized.write_to(&mut buffer, ImageOutputFormat::WebP) {
        return HttpResponse::InternalServerError().body("Failed to encode image");
    }

    return HttpResponse::Ok()
        .content_type("image/webp")
        .insert_header((CACHE_CONTROL, "public, max-age=2592000"))
        .insert_header((LAST_MODIFIED, HttpDate::from(modified_time).to_string()))
        .body(buffer.into_inner());
}

fn load_image(path: &Path) -> Result<DynamicImage, ImageError> {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "psd" => return load_image_from_psd(path),
        _ => return load_image_from_file(path),
    }
}

fn load_image_from_file(path: &Path) -> Result<DynamicImage, ImageError> {
    ImageReader::open(&path)?.decode()
}

fn load_image_from_psd(path: &Path) -> Result<DynamicImage, ImageError> {
    let bytes = std::fs::read(&path)?;
    let psd = Psd::from_bytes(&bytes).map_err(|err| {
        image::ImageError::Decoding(image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            format!("Failed to parse PSD: {}", err),
        ))
    })?;

    let rgba = psd.rgba();
    let width = psd.width();
    let height = psd.height();

    let img_buf = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| {
            image::ImageError::Limits(image::error::LimitError::from_kind(
                image::error::LimitErrorKind::DimensionError,
            ))
        })?;
    Ok(DynamicImage::ImageRgba8(img_buf))
}

#[derive(Parser)]
#[command(name = "media-thumb-server")]
#[command(about = "Serve thumbnails from NAS")]
struct Args {
    /// Base path to the NAS media directory
    #[arg(long)]
    base_path: PathBuf,

    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    #[arg(short, long, default_value_t = 8080)]
    port: u16,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("INFO"));

    let args = Args::parse();

    log::info!("Starting HTTP server at http://{}:{}", args.bind, args.port);

    let base_path = args.base_path.canonicalize().expect("Invalid base path");

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(web::Data::new(base_path.clone()))
            .service(thumbnail)
            .service(media)
            .service(original)
    })
    .bind((args.bind.as_str(), args.port))?
    .run()
    .await
}

use actix_files as fs;
use actix_web::http::header;
use actix_web::http::StatusCode;
use actix_web::{
    get, middleware::Logger, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
    ResponseError,
};
use clap::Parser;
use image::error::ImageError;
use image::{ColorType, DynamicImage};
use psd::Psd;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use webp::Encoder;
mod movie_keyframe;
mod statistics;

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
    FailedToEncode(String),

    #[error("Failed to encode: err={0}")]
    FailedToDecodeMovie(anyhow::Error),
}

impl ResponseError for ApiError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        match self {
            ApiError::NotFound() => StatusCode::NOT_FOUND,
            ApiError::InvalidKey(_) => StatusCode::NOT_FOUND,
            ApiError::FailedToDecode(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::FailedToEncode(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::FailedToDecodeMovie(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
    if let Some(ims) = req.headers().get(header::IF_MODIFIED_SINCE) {
        if let Ok(ims_str) = ims.to_str() {
            if let Ok(ims_time) = httpdate::parse_http_date(ims_str) {
                return modified_time <= ims_time;
            }
        }
    }
    false
}

#[get("/raw/{tail:.*}")]
async fn original(
    _req: HttpRequest,
    path: web::Path<String>,
    app_data: web::Data<AppData>,
) -> Result<fs::NamedFile, Error> {
    let canonical_path = path_from_key(app_data.base_path.as_path(), &path.into_inner())?;

    let named_file = fs::NamedFile::open(canonical_path)?;
    Ok(named_file
        .use_last_modified(true)
        .set_content_disposition(header::ContentDisposition {
            disposition: header::DispositionType::Attachment,
            parameters: vec![],
        }))
}

#[get("/media/{tail:.*}")]
async fn media(
    req: HttpRequest,
    path: web::Path<String>,
    app_data: web::Data<AppData>,
) -> Result<impl Responder, Error> {
    let canonical_path = path_from_key(app_data.base_path.as_path(), &path.into_inner())?;

    // Check Last Modified header
    let modified_time = std::fs::metadata(&canonical_path)?
        .modified()
        .unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return Ok(HttpResponse::NotModified().finish());
    }

    let img = load_image(&canonical_path, &app_data.config.load_image_option)?;
    Ok(build_webp_response(
        img,
        &canonical_path,
        modified_time,
        app_data.config.media_quality,
    )?)
}

#[get("/thumbnail/{tail:.*}")]
async fn thumbnail(
    req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    app_data: web::Data<AppData>,
) -> Result<impl Responder, Error> {
    let size = query
        .get("size")
        .map(|s| Size::from_str(s))
        .unwrap_or(Size::Medium);
    let canonical_path = path_from_key(app_data.base_path.as_path(), &path.into_inner())?;

    // Check Last Modified header
    let modified_time = std::fs::metadata(&canonical_path)?
        .modified()
        .unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return Ok(HttpResponse::NotModified().finish());
    }

    let img = load_image(&canonical_path, &app_data.config.load_image_option)?;
    let (w, h) = size.dimensions();
    let resized = img.thumbnail(w, h);
    Ok(build_webp_response(
        resized,
        &canonical_path,
        modified_time,
        app_data.config.thumbnail_quality,
    )?)
}

fn load_image(path: &Path, option: &LoadImageOption) -> Result<DynamicImage, ApiError> {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "psd" => load_image_from_psd(path).map_err(ApiError::FailedToDecode),
        "mp4" | "webm" | "mov" => movie_keyframe::load_image_from_movie_keyframe(
            path,
            option.movie_max_keyframes,
            option.movie_frame_score_threshold,
            option.movie_frame_sharpness_threshold,
        )
        .map_err(ApiError::FailedToDecodeMovie),
        _ => load_image_from_file(path).map_err(ApiError::FailedToDecode),
    }
}

fn load_image_from_file(path: &Path) -> Result<DynamicImage, ImageError> {
    image::ImageReader::open(path)?.decode()
}

fn load_image_from_psd(path: &Path) -> Result<DynamicImage, ImageError> {
    let bytes = std::fs::read(path)?;
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

fn build_webp_response(
    img: DynamicImage,
    path: &Path,
    modified_time: SystemTime,
    quality: f32,
) -> Result<HttpResponse, ApiError> {
    let rgba8 = match img.color() {
        ColorType::Rgb32F => DynamicImage::ImageRgb8(img.to_rgb8()),
        ColorType::Rgba32F => DynamicImage::ImageRgba8(img.to_rgba8()),
        ColorType::Rgb16 => DynamicImage::ImageRgb8(img.to_rgb8()),
        ColorType::Rgba16 => DynamicImage::ImageRgba8(img.to_rgba8()),
        ColorType::Rgb8 | ColorType::Rgba8 => img,
        _ => DynamicImage::ImageRgb8(img.to_rgb8()),
    };

    let encoder = Encoder::from_image(&rgba8).map_err(|err| {
        log::warn!(
            "Failed to encode image: {}:{}",
            path.to_str().unwrap_or("N/A"),
            err,
        );
        ApiError::FailedToEncode(err.to_string())
    })?;
    let webp_data = encoder.encode(quality);

    Ok(HttpResponse::Ok()
        .content_type("image/webp")
        .insert_header(header::CacheControl(vec![
            header::CacheDirective::Public,
            header::CacheDirective::MaxAge(2592000u32),
        ]))
        .insert_header(header::LastModified(modified_time.into()))
        .body(webp_data.to_vec()))
}

#[derive(Parser)]
#[command(name = "media-thumb-server")]
#[command(about = "Serve thumbnails from NAS")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    base_path: PathBuf,

    #[command(flatten)]
    config: AppConfig,
}

#[derive(Parser)]
struct AppConfig {
    #[arg(short, long, default_value_t = 95.0)]
    thumbnail_quality: f32,

    #[arg(short, long, default_value_t = 97.0)]
    media_quality: f32,

    #[command(flatten)]
    load_image_option: LoadImageOption,
}

#[derive(Parser)]
struct LoadImageOption {
    #[arg(short, long, default_value_t = 10)]
    movie_max_keyframes: i32,

    #[arg(short, long, default_value_t = 1.0)]
    movie_frame_score_threshold: f32,

    #[arg(short, long)]
    movie_frame_sharpness_threshold: Option<f32>,
}

struct AppData {
    base_path: PathBuf,
    config: AppConfig,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("INFO"));

    let args = Args::parse();
    let base_path = args.base_path.canonicalize().expect("Invalid base path");
    let app_data = web::Data::new(AppData {
        base_path,
        config: args.config,
    });

    log::info!("Starting HTTP server at http://{}:{}", args.bind, args.port);

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(app_data.clone())
            .service(thumbnail)
            .service(media)
            .service(original)
    })
    .bind((args.bind.as_str(), args.port))?
    .run()
    .await
}

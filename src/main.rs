use actix_files as fs;
use actix_web::http::header;
use actix_web::http::StatusCode;
use actix_web::{
    get, middleware::Logger, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
    ResponseError,
};
use clap::Parser;
use ffmpeg_next::{
    codec, format, frame::Video, media::Type, software::scaling,
    software::scaling::context::Context as ScalingContext, util::format::pixel::Pixel,
};
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
    base_path: web::Data<std::path::PathBuf>,
) -> Result<fs::NamedFile, Error> {
    let canonical_path = path_from_key(base_path.get_ref(), &path.into_inner())?;

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
    base_path: web::Data<std::path::PathBuf>,
) -> Result<impl Responder, Error> {
    let canonical_path = path_from_key(base_path.get_ref(), &path.into_inner())?;

    // Check Last Modified header
    let modified_time = std::fs::metadata(&canonical_path)?
        .modified()
        .unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return Ok(HttpResponse::NotModified().finish());
    }

    let img = load_image_handle_api_error(&canonical_path)?;
    Ok(build_webp_response(img, &canonical_path, modified_time)?)
}

#[get("/thumbnail/{tail:.*}")]
async fn thumbnail(
    req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    base_path: web::Data<std::path::PathBuf>,
) -> Result<impl Responder, Error> {
    let size = query
        .get("size")
        .map(|s| Size::from_str(s))
        .unwrap_or(Size::Medium);
    let canonical_path = path_from_key(base_path.get_ref(), &path.into_inner())?;

    // Check Last Modified header
    let modified_time = std::fs::metadata(&canonical_path)?
        .modified()
        .unwrap_or(SystemTime::now());
    if is_not_modified(&req, modified_time) {
        return Ok(HttpResponse::NotModified().finish());
    }

    let img = load_image_handle_api_error(&canonical_path)?;
    let (w, h) = size.dimensions();
    let resized = img.thumbnail(w, h);
    Ok(build_webp_response(
        resized,
        &canonical_path,
        modified_time,
    )?)
}

fn load_image_handle_api_error(path: &Path) -> Result<DynamicImage, ApiError> {
    load_image(path).map_err(ApiError::FailedToDecode)
}

fn load_image(path: &Path) -> Result<DynamicImage, ImageError> {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "psd" => load_image_from_psd(path),
        "mp4" | "webm" | "mov" => load_image_from_movie_keyframe(path),
        _ => load_image_from_file(path),
    }
}

fn load_image_from_file(path: &Path) -> Result<DynamicImage, ImageError> {
    ImageReader::open(path)?.decode()
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

fn load_image_from_movie_keyframe(path: &Path) -> Result<DynamicImage, ImageError> {
    ffmpeg_next::init().ok(); // すでに初期化済なら何もしない

    let mut ictx = format::input(&path).map_err(|_| {
        image::ImageError::Decoding(image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            "Failed to open video",
        ))
    })?;

    let input = ictx.streams().best(Type::Video).ok_or_else(|| {
        image::ImageError::Decoding(image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            "No video stream found",
        ))
    })?;

    let video_stream_index = input.index();
    let codec_params = input.parameters();
    let context = codec::Context::from_parameters(codec_params).map_err(|_| {
        image::ImageError::Decoding(image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            "Failed to get codec context",
        ))
    })?;
    let mut decoder = context.decoder().video().map_err(|_| {
        image::ImageError::Decoding(image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            "Failed to get video decoder",
        ))
    })?;

    let mut scaler = ScalingContext::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        Pixel::RGB24,
        decoder.width(),
        decoder.height(),
        scaling::Flags::BILINEAR,
    )
    .unwrap();

    for (stream, packet) in ictx.packets() {
        if stream.index() == video_stream_index {
            decoder.send_packet(&packet).ok();

            let mut decoded = Video::empty();
            while decoder.receive_frame(&mut decoded).is_ok() {
                let mut rgb_frame = Video::empty();
                scaler.run(&decoded, &mut rgb_frame).unwrap();

                let width = rgb_frame.width() as usize;
                let height = rgb_frame.height() as usize;
                let stride = rgb_frame.stride(0);
                let data = rgb_frame.data(0);

                let mut buffer = Vec::with_capacity(width * height * 3);
                for y in 0..height {
                    let row_start = y * stride;
                    buffer.extend_from_slice(&data[row_start..row_start + width * 3]);
                }

                let image_buffer = image::RgbImage::from_raw(width as u32, height as u32, buffer)
                    .ok_or_else(|| {
                    image::ImageError::Limits(image::error::LimitError::from_kind(
                        image::error::LimitErrorKind::DimensionError,
                    ))
                })?;
                return Ok(DynamicImage::ImageRgb8(image_buffer));
            }
        }
    }

    Err(image::ImageError::Decoding(
        image::error::DecodingError::new(
            image::error::ImageFormatHint::Unknown,
            "No frame extracted",
        ),
    ))
}

fn build_webp_response(
    img: DynamicImage,
    path: &Path,
    modified_time: SystemTime,
) -> Result<HttpResponse, ApiError> {
    let mut buffer = Cursor::new(Vec::new());
    if let Err(err) = img.write_to(&mut buffer, ImageOutputFormat::WebP) {
        log::warn!(
            "Failed to encode image: {}:{}",
            path.to_str().unwrap_or("N/A"),
            err,
        );
        return Err(ApiError::FailedToEncode(err));
    }

    Ok(HttpResponse::Ok()
        .content_type("image/webp")
        .insert_header(header::CacheControl(vec![
            header::CacheDirective::Public,
            header::CacheDirective::MaxAge(2592000u32),
        ]))
        .insert_header(header::LastModified(modified_time.into()))
        .body(buffer.into_inner()))
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

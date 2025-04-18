use actix_web::http::header::{CACHE_CONTROL, IF_MODIFIED_SINCE, LAST_MODIFIED};
use actix_web::{
    get, middleware::Logger, web, App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use clap::Parser;
use httpdate::HttpDate;
use image::io::Reader as ImageReader;
use image::{DynamicImage, ImageOutputFormat};
use std::io::Cursor;
use std::path::PathBuf;
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

#[get("/thumbnail/{tail:.*}")]
async fn thumbnail(
    req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
    base_path: web::Data<std::path::PathBuf>,
) -> impl Responder {
    let rel_path = path.into_inner();
    let size = query
        .get("size")
        .map(|s| Size::from_str(s))
        .unwrap_or(Size::Medium);

    let full_path = base_path.join(&rel_path);

    // パストラバーサル防止
    let canonical_base = base_path;
    let canonical_path = match std::fs::canonicalize(&full_path) {
        Ok(p) => p,
        Err(_) => return HttpResponse::NotFound().body("File not found"),
    };

    if !canonical_path.starts_with(&**canonical_base) {
        return HttpResponse::Forbidden().body("Access denied");
    }

    // ファイルの最終更新日時
    let metadata = match std::fs::metadata(&canonical_path) {
        Ok(meta) => meta,
        Err(_) => return HttpResponse::InternalServerError().body("Failed to read metadata"),
    };

    let modified_time = metadata.modified().unwrap_or(SystemTime::now());

    // If-Modified-Since ヘッダ処理
    if let Some(ims) = req.headers().get(IF_MODIFIED_SINCE) {
        if let Ok(ims_str) = ims.to_str() {
            if let Ok(ims_time) = httpdate::parse_http_date(ims_str) {
                if modified_time <= ims_time {
                    return HttpResponse::NotModified().finish();
                }
            }
        }
    }

    // 画像読み込みとリサイズ
    let img_reader_result = ImageReader::open(&canonical_path);
    let img_result = match img_reader_result {
        Ok(img_reader) => img_reader.decode(),
        Err(_) => return HttpResponse::InternalServerError().body("Failed to open image"),
    };

    let resized = match img_result {
        Ok(img) => resize_image(img, size),
        Err(_) => return HttpResponse::InternalServerError().body("Failed to decode image"),
    };

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

fn resize_image(img: DynamicImage, size: Size) -> DynamicImage {
    let (w, h) = size.dimensions();
    img.thumbnail(w, h)
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
    })
    .bind((args.bind.as_str(), args.port))?
    .run()
    .await
}

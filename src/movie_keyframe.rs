use anyhow::{Context, Result};
use ffmpeg::codec;
use ffmpeg::format::input;
use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags};
use ffmpeg::util::frame::video::Video as FfmpegFrame;
use ffmpeg_next as ffmpeg;
use image::{DynamicImage, ImageBuffer, Rgb};
use scopeguard::guard;
use std::path::Path;

pub fn load_image_from_movie_keyframe(
    path: &Path,
    max_frames: i32,
    threshold_score: f32,
) -> Result<DynamicImage, anyhow::Error> {
    ffmpeg::init().ok(); // Ignore re-init

    let mut ictx = input(&path)?;
    let input = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("No video stream found")?;
    let video_stream_index = input.index();

    let codec_params = input.parameters();
    let context_decoder = codec::Context::from_parameters(codec_params)?;

    let decoder_bare = context_decoder.decoder().video()?;
    let mut decoder = guard(decoder_bare, |mut decoder| {
        log::debug!("{}: flush remaining packets", path.display());
        decoder.send_eof().unwrap_or_else(|err| {
            log::debug!("{}: failed to flush: {}", path.display(), err);
        })
    });

    let mut scaler = ScalingContext::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGB24,
        decoder.width(),
        decoder.height(),
        Flags::BILINEAR,
    )?;

    let mut best_frame: Option<DynamicImage> = None;
    let mut best_score = -1.0_f32;

    let mut frame_index = 0;

    for (stream, packet) in ictx.packets() {
        if stream.index() != video_stream_index {
            continue;
        }

        decoder.send_packet(&packet)?;

        let mut decoded = FfmpegFrame::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let mut rgb_frame = FfmpegFrame::empty();
            scaler.run(&decoded, &mut rgb_frame)?;

            let image = frame_to_dynamic_image(&rgb_frame)?;
            let score = compute_frame_score(&image);
            log::debug!(
                "{}[{}]: Frame score: {}",
                path.display(),
                frame_index,
                score
            );

            if score >= threshold_score {
                return Ok(image);
            }

            if score > best_score {
                best_score = score;
                best_frame = Some(image);
            }

            frame_index += 1;
            if frame_index >= max_frames {
                break;
            }
        }

        if frame_index >= max_frames {
            break;
        }
    }

    best_frame.ok_or_else(|| anyhow::anyhow!("No suitable frame found"))
}

fn frame_to_dynamic_image(frame: &FfmpegFrame) -> Result<DynamicImage, anyhow::Error> {
    let width = frame.width();
    let height = frame.height();
    let data = frame.data(0);
    let stride = frame.stride(0);

    let mut buf = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        let offset = (y as usize) * stride;
        buf.extend_from_slice(&data[offset..offset + (width as usize * 3)]);
    }

    let image = ImageBuffer::<Rgb<u8>, _>::from_raw(width, height, buf)
        .ok_or_else(|| anyhow::anyhow!("Failed to build ImageBuffer"))?;

    Ok(DynamicImage::ImageRgb8(image))
}

fn compute_frame_score(image: &DynamicImage) -> f32 {
    let rgb = image.to_rgb8();
    let mut brightness = Vec::with_capacity((rgb.width() * rgb.height()) as usize);

    for pixel in rgb.pixels() {
        let [r, g, b] = pixel.0;
        let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        brightness.push(luma);
    }

    let mean = brightness.iter().sum::<f32>() / brightness.len() as f32;
    let stddev = (brightness.iter().map(|v| (v - mean).powi(2)).sum::<f32>()
        / brightness.len() as f32)
        .sqrt();

    // スコア: 平均輝度が極端でなく、分散がある（情報量がある）画像を評価
    stddev * (1.0 - (mean - 128.0).abs() / 128.0)
}

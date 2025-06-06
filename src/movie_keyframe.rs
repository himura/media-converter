use crate::statistics;
use anyhow::{Context, Result};
use ffmpeg::codec;
use ffmpeg::format::input;
use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags};
use ffmpeg::util::frame::video::Video as FfmpegFrame;
use ffmpeg_next as ffmpeg;
use image::{DynamicImage, GrayImage, ImageBuffer, Rgb};
use scopeguard::guard;
use std::path::Path;

pub fn load_image_from_movie_keyframe(
    path: &Path,
    max_keyframes: i32,
    threshold_score: f32,
    threshold_sharpness: Option<f32>,
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
            if decoded.is_key() {
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
                    if let Some(threshold) = threshold_sharpness {
                        let sharpness = compute_frame_sharpness(&image) as f32;
                        log::debug!(
                            "{}[{}]: Frame sharpness: {}",
                            path.display(),
                            frame_index,
                            sharpness
                        );
                        if sharpness >= threshold {
                            return Ok(image);
                        }
                    } else {
                        return Ok(image);
                    }
                }

                if score > best_score {
                    best_score = score;
                    best_frame = Some(image);
                }

                frame_index += 1;
                if frame_index >= max_keyframes {
                    break;
                }
            }
        }

        if frame_index >= max_keyframes {
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
    let mut brightness_stats = statistics::OnlineStats::new();
    let mut saturation_stats = statistics::OnlineStats::new();

    for pixel in rgb.pixels() {
        let [r, g, b] = pixel.0;

        // 明度 (Luma: Y)
        // TODO: HSV の V で良い説
        let luma = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
        brightness_stats.update(luma);

        // 彩度 (HSV の S)
        let rf = r as f64 / 255.0;
        let gf = g as f64 / 255.0;
        let bf = b as f64 / 255.0;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let saturation = if max == 0.0 { 0.0 } else { (max - min) / max };
        saturation_stats.update(saturation);
    }

    let brightness_penalty = 1.0 - ((brightness_stats.mean() - 128.0).abs() / 128.0);

    (brightness_stats.stddev() * saturation_stats.mean() * brightness_penalty) as f32
}

fn compute_frame_sharpness(image: &DynamicImage) -> f64 {
    let gray: GrayImage = image.to_luma8();

    let lap = imageproc::filter::laplacian_filter(&gray);

    let mut stats = statistics::OnlineStats::new();
    for pixel in lap.pixels() {
        let v = pixel[0] as f64;
        stats.update(v);
    }

    stats.variance()
}

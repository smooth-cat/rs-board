use std::{fs, io::Cursor, path::Path};

use image::{DynamicImage, ImageFormat, RgbaImage, imageops::FilterType};
use thiserror::Error;

#[cfg(not(target_os = "macos"))]
use {
  arboard::{Clipboard, ImageData},
  std::borrow::Cow,
};

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, ExportError> {
  let mut output = Cursor::new(Vec::new());
  DynamicImage::ImageRgba8(image.clone()).write_to(&mut output, ImageFormat::Png)?;
  Ok(output.into_inner())
}

pub fn encode_tiff(image: &RgbaImage) -> Result<Vec<u8>, ExportError> {
  let mut output = Cursor::new(Vec::new());
  DynamicImage::ImageRgba8(image.clone()).write_to(&mut output, ImageFormat::Tiff)?;
  Ok(output.into_inner())
}

pub fn write_png_atomically(path: &Path, image: &RgbaImage) -> Result<(), ExportError> {
  let parent = path.parent().ok_or(ExportError::InvalidDestination)?;
  fs::create_dir_all(parent)?;
  let file_name =
    path.file_name().and_then(|name| name.to_str()).ok_or(ExportError::InvalidDestination)?;
  let temporary = parent.join(format!(".{file_name}.tmp"));
  let result = (|| {
    fs::write(&temporary, encode_png(image)?)?;
    fs::File::open(&temporary)?.sync_all()?;
    fs::rename(&temporary, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok::<_, ExportError>(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(&temporary);
  }
  result
}

pub fn copy_image(image: &RgbaImage) -> Result<(), ExportError> {
  #[cfg(target_os = "macos")]
  {
    copy_image_to_macos_pasteboard(image)
  }

  #[cfg(not(target_os = "macos"))]
  {
    copy_image_to_generic_clipboard(image)
  }
}

#[cfg(not(target_os = "macos"))]
fn copy_image_to_generic_clipboard(image: &RgbaImage) -> Result<(), ExportError> {
  let mut clipboard = Clipboard::new()?;
  clipboard.set_image(ImageData {
    width: image.width() as usize,
    height: image.height() as usize,
    bytes: Cow::Owned(image.as_raw().clone()),
  })?;
  Ok(())
}

#[cfg(target_os = "macos")]
fn copy_image_to_macos_pasteboard(image: &RgbaImage) -> Result<(), ExportError> {
  use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF};
  use objc2_foundation::NSData;

  let png = encode_png(image)?;
  let tiff = encode_tiff(image)?;

  let pasteboard = NSPasteboard::generalPasteboard();
  pasteboard.clearContents();
  let png_data = unsafe { NSData::dataWithBytes_length(png.as_ptr().cast(), png.len()) };
  let tiff_data = unsafe { NSData::dataWithBytes_length(tiff.as_ptr().cast(), tiff.len()) };
  let wrote_png = pasteboard.setData_forType(Some(&png_data), unsafe { NSPasteboardTypePNG });
  let wrote_tiff = pasteboard.setData_forType(Some(&tiff_data), unsafe { NSPasteboardTypeTIFF });
  if wrote_png && wrote_tiff {
    Ok(())
  } else {
    Err(ExportError::Pasteboard(
      "NSPasteboard refused one or more image representations".to_owned(),
    ))
  }
}

pub fn make_preview(image: &RgbaImage, maximum_long_edge: u32) -> RgbaImage {
  let long_edge = image.width().max(image.height());
  if long_edge <= maximum_long_edge {
    return image.clone();
  }
  let scale = maximum_long_edge as f64 / long_edge as f64;
  let width = (image.width() as f64 * scale).round().max(1.0) as u32;
  let height = (image.height() as f64 * scale).round().max(1.0) as u32;
  image::imageops::resize(image, width, height, FilterType::Lanczos3)
}

#[derive(Debug, Error)]
pub enum ExportError {
  #[error("导出目标路径无效")]
  InvalidDestination,
  #[error("图片编码失败: {0}")]
  Image(#[from] image::ImageError),
  #[error("文件写入失败: {0}")]
  Io(#[from] std::io::Error),
  #[error("剪贴板写入失败: {0}")]
  Clipboard(#[from] arboard::Error),
  #[error("剪贴板写入失败: {0}")]
  Pasteboard(String),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn preview_uses_a_480px_long_edge_without_cropping() {
    let image = RgbaImage::new(3_840, 2_160);
    let preview = make_preview(&image, 480);
    assert_eq!(preview.dimensions(), (480, 270));
  }

  #[test]
  fn smaller_preview_is_not_upscaled() {
    let image = RgbaImage::new(320, 200);
    assert_eq!(make_preview(&image, 480).dimensions(), (320, 200));
  }

  #[test]
  fn encodes_png_and_tiff_clipboard_representations() {
    let image = RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
    let png = encode_png(&image).unwrap();
    let tiff = encode_tiff(&image).unwrap();
    let decoded_png = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    let decoded_tiff = image::load_from_memory_with_format(&tiff, ImageFormat::Tiff).unwrap();
    assert_eq!((decoded_png.width(), decoded_png.height()), (2, 1));
    assert_eq!((decoded_tiff.width(), decoded_tiff.height()), (2, 1));
  }
}

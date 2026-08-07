use std::{io::Cursor, sync::Arc};

use image::{DynamicImage, ImageFormat, ImageReader, RgbaImage};

use super::{StorageError, StorageResult};

pub(crate) const MAX_CANVAS_DIMENSION_PX: u32 = 8_192;
pub(crate) const MAX_ENCODED_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum BackgroundData {
  Rgba8 { width_px: u32, height_px: u32, pixels: Arc<[u8]> },
  EncodedPng(Arc<[u8]>),
}

impl BackgroundData {
  pub fn rgba8(width_px: u32, height_px: u32, pixels: impl Into<Arc<[u8]>>) -> StorageResult<Self> {
    validate_dimensions(width_px, height_px)?;
    let pixels = pixels.into();
    validate_rgba_length(width_px, height_px, pixels.len())?;
    Ok(Self::Rgba8 { width_px, height_px, pixels })
  }

  pub fn encoded_png(bytes: impl Into<Arc<[u8]>>) -> StorageResult<Self> {
    let bytes = bytes.into();
    if bytes.len() as u64 > MAX_ENCODED_IMAGE_BYTES {
      return Err(StorageError::InvalidImage("encoded image exceeds the storage limit".into()));
    }
    inspect_png_dimensions(&bytes)?;
    Ok(Self::EncodedPng(bytes))
  }

  pub fn dimensions(&self) -> StorageResult<(u32, u32)> {
    match self {
      Self::Rgba8 { width_px, height_px, .. } => Ok((*width_px, *height_px)),
      Self::EncodedPng(bytes) => inspect_png_dimensions(bytes),
    }
  }

  pub fn decode_rgba8(&self) -> StorageResult<(u32, u32, Arc<[u8]>)> {
    match self {
      Self::Rgba8 { width_px, height_px, pixels } => {
        Ok((*width_px, *height_px, Arc::clone(pixels)))
      }
      Self::EncodedPng(bytes) => {
        let image = decode_png(bytes)?;
        let (width_px, height_px) = image.dimensions();
        Ok((width_px, height_px, Arc::from(image.into_raw())))
      }
    }
  }

  pub fn encoded_png_bytes(&self) -> Option<Arc<[u8]>> {
    match self {
      Self::EncodedPng(bytes) => Some(Arc::clone(bytes)),
      Self::Rgba8 { .. } => None,
    }
  }

  pub(crate) fn normalized_png(
    &self,
    expected_width_px: u32,
    expected_height_px: u32,
  ) -> StorageResult<Arc<[u8]>> {
    validate_dimensions(expected_width_px, expected_height_px)?;
    let (width_px, height_px) = self.dimensions()?;
    if (width_px, height_px) != (expected_width_px, expected_height_px) {
      return Err(StorageError::InvalidImage(format!(
        "background dimensions are {width_px}x{height_px}, expected {expected_width_px}x{expected_height_px}"
      )));
    }

    let rgba = match self {
      Self::Rgba8 { pixels, .. } => {
        RgbaImage::from_raw(expected_width_px, expected_height_px, pixels.to_vec())
          .ok_or_else(|| StorageError::InvalidImage("invalid RGBA pixel buffer".into()))?
      }
      Self::EncodedPng(bytes) => return Ok(Arc::clone(bytes)),
    };
    encode_rgba_png(rgba).map(Arc::from)
  }
}

pub(crate) fn inspect_png_dimensions(bytes: &[u8]) -> StorageResult<(u32, u32)> {
  if bytes.len() as u64 > MAX_ENCODED_IMAGE_BYTES {
    return Err(StorageError::InvalidImage("encoded image exceeds the storage limit".into()));
  }
  let reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
  let dimensions =
    reader.into_dimensions().map_err(|error| StorageError::InvalidImage(error.to_string()))?;
  validate_dimensions(dimensions.0, dimensions.1)?;
  Ok(dimensions)
}

fn decode_png(bytes: &[u8]) -> StorageResult<RgbaImage> {
  inspect_png_dimensions(bytes)?;
  image::load_from_memory_with_format(bytes, ImageFormat::Png)
    .map(DynamicImage::into_rgba8)
    .map_err(|error| StorageError::InvalidImage(error.to_string()))
}

fn encode_rgba_png(image: RgbaImage) -> StorageResult<Vec<u8>> {
  let mut bytes = Cursor::new(Vec::new());
  DynamicImage::ImageRgba8(image)
    .write_to(&mut bytes, ImageFormat::Png)
    .map_err(|error| StorageError::InvalidImage(error.to_string()))?;
  Ok(bytes.into_inner())
}

fn validate_dimensions(width_px: u32, height_px: u32) -> StorageResult<()> {
  if width_px == 0 || height_px == 0 {
    return Err(StorageError::InvalidImage("background dimensions must be non-zero".into()));
  }
  if width_px > MAX_CANVAS_DIMENSION_PX || height_px > MAX_CANVAS_DIMENSION_PX {
    return Err(StorageError::InvalidImage(format!(
      "background exceeds {MAX_CANVAS_DIMENSION_PX}px"
    )));
  }
  Ok(())
}

fn validate_rgba_length(width_px: u32, height_px: u32, actual: usize) -> StorageResult<()> {
  let expected = (width_px as usize)
    .checked_mul(height_px as usize)
    .and_then(|pixels| pixels.checked_mul(4))
    .ok_or_else(|| StorageError::InvalidImage("RGBA dimensions overflow".into()))?;
  if actual != expected {
    return Err(StorageError::InvalidImage(format!(
      "RGBA buffer has {actual} bytes, expected {expected}"
    )));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rgba_background_round_trips_through_png() {
    let background = BackgroundData::rgba8(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
    let png = background.normalized_png(2, 1).unwrap();
    let encoded = BackgroundData::encoded_png(png).unwrap();
    let (_, _, pixels) = encoded.decode_rgba8().unwrap();
    assert_eq!(&*pixels, &[255, 0, 0, 255, 0, 255, 0, 255]);
  }

  #[test]
  fn mismatched_rgba_length_is_rejected() {
    assert!(BackgroundData::rgba8(2, 2, vec![0; 15]).is_err());
  }

  #[test]
  fn expected_dimensions_are_enforced() {
    let background = BackgroundData::rgba8(1, 1, vec![0; 4]).unwrap();
    assert!(background.normalized_png(2, 1).is_err());
  }

  #[test]
  fn validated_encoded_png_is_reused_without_reencoding() {
    let encoded =
      BackgroundData::rgba8(1, 1, vec![10, 20, 30, 255]).unwrap().normalized_png(1, 1).unwrap();
    let background = BackgroundData::encoded_png(Arc::clone(&encoded)).unwrap();
    let normalized = background.normalized_png(1, 1).unwrap();
    assert!(Arc::ptr_eq(&encoded, &normalized));
  }
}

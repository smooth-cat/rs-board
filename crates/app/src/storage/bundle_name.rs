use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundleNames {
  pub stem: String,
  pub manifest: PathBuf,
  pub background: PathBuf,
  pub preview: PathBuf,
}

pub(crate) fn choose_available_bundle_names(directory: &Path, title: &str) -> BundleNames {
  let base = sanitize_file_stem(title);
  for suffix in 1u32.. {
    let stem = if suffix == 1 { base.clone() } else { format!("{base}-{suffix}") };
    let names = BundleNames {
      manifest: directory.join(format!("{stem}.rsboard")),
      background: directory.join(format!("{stem}.png")),
      preview: directory.join(format!("{stem}.preview.png")),
      stem,
    };
    if !names.manifest.exists() && !names.background.exists() && !names.preview.exists() {
      return names;
    }
  }
  unreachable!("u32 export suffixes cannot be exhausted in practice")
}

pub(crate) fn sanitize_file_stem(title: &str) -> String {
  let mut output = String::with_capacity(title.len().min(120));
  let mut previous_was_space = false;
  for character in title.trim().chars() {
    let character = match character {
      '/' | '\\' | ':' => '_',
      value if value.is_control() => continue,
      value if value.is_whitespace() => ' ',
      value => value,
    };
    if character == ' ' {
      if previous_was_space {
        continue;
      }
      previous_was_space = true;
    } else {
      previous_was_space = false;
    }

    if output.len() + character.len_utf8() > 120 {
      break;
    }
    output.push(character);
  }
  let output = output.trim_matches([' ', '.']).to_owned();
  if output.is_empty() { "未命名讲义".to_owned() } else { output }
}

#[cfg(test)]
mod tests {
  use uuid::Uuid;

  use super::*;

  #[test]
  fn title_sanitization_removes_path_syntax_and_controls() {
    assert_eq!(sanitize_file_stem(" ../课程/第一节:\n "), "_课程_第一节_");
    assert_eq!(sanitize_file_stem(" . "), "未命名讲义");
  }

  #[test]
  fn bundle_collision_uses_one_suffix_for_all_resources() {
    let root = std::env::temp_dir().join(format!("rs-board-names-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("课程.png"), []).unwrap();
    let names = choose_available_bundle_names(&root, "课程");
    assert_eq!(names.stem, "课程-2");
    assert!(names.manifest.ends_with("课程-2.rsboard"));
    assert!(names.background.ends_with("课程-2.png"));
    std::fs::remove_dir_all(root).unwrap();
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub label: &'static str,
    pub current: u64,
    pub total: Option<u64>,
}

impl Update {
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total?;
        (total > 0 && self.current <= total).then_some(self.current as f32 / total as f32)
    }

    pub fn message(&self) -> String {
        match self.total {
            Some(total) => format!("{} — {}/{}", self.label, self.current, total),
            None => format!("{} — {}", self.label, self.current),
        }
    }
}

pub fn parse(line: &str) -> Option<Update> {
    for (needle, label, bracketed) in [
        ("Processed file [", "Feature extraction", true),
        ("Processing image [", "Feature matching", true),
        ("Indexing image [", "Feature indexing", true),
        ("Undistorting image [", "Image undistortion", true),
        ("Fusing image [", "Dense fusion", true),
        ("Processing view ", "PatchMatch stereo", false),
    ] {
        if let Some(start) = line.find(needle) {
            let rest = &line[start + needle.len()..];
            let end = if bracketed {
                rest.find(']')?
            } else {
                rest.find(" for ").unwrap_or(rest.len())
            };
            let pair = rest[..end].replace(' ', "");
            let (current, total) = pair.split_once('/')?;
            let current = current.parse().ok()?;
            let total = total.parse().ok()?;
            if total > 0 && current <= total {
                return Some(Update {
                    label,
                    current,
                    total: Some(total),
                });
            }
        }
    }
    if let Some(start) = line.find("num_reg_frames=") {
        let digits: String = line[start + "num_reg_frames=".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(current) = digits.parse() {
            return Some(Update {
                label: "Sparse mapping: registered frames",
                current,
                total: None,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colmap_progress_without_inventing_totals() {
        assert_eq!(
            parse("I timer] Processed file [12/128]"),
            Some(Update {
                label: "Feature extraction",
                current: 12,
                total: Some(128)
            })
        );
        assert_eq!(
            parse("Processing view 4 / 17 for image.jpg")
                .unwrap()
                .fraction(),
            Some(4.0 / 17.0)
        );
        assert_eq!(
            parse("Registering image #8 (num_reg_frames=37)")
                .unwrap()
                .total,
            None
        );
        assert!(parse("unrelated log output").is_none());
        assert!(parse("Processed file [9/4]").is_none());
    }
}

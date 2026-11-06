//! Bounded wire updates for a complete, locally retained remote chat snapshot.
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frame {
    base: String,
    start: usize,
    end: usize,
    offset: usize,
    data: String,
    done: bool,
    #[serde(default)]
    patch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub(crate) fn frames(before: &str, after: &str) -> anyhow::Result<Vec<Frame>> {
    ensure!(
        after.len() <= MAX_HISTORY_BYTES,
        "Remote history exceeds assembly limit"
    );
    if let (Ok(old), Ok(new)) = (
        serde_json::from_str::<serde_json::Value>(before),
        serde_json::from_str::<serde_json::Value>(after),
    ) {
        let mut edits = Vec::new();
        diff(&old, &new, &mut Vec::new(), &mut edits);
        let data = serde_json::to_string(&edits)?;
        if data.len() > after.len() || data.len() > MAX_HISTORY_BYTES {
            return splice_frames(before, after);
        }
        let mut chunks = splice_frames("", &data)?;
        let base = digest(before);
        for chunk in &mut chunks {
            chunk.base = base.clone();
            chunk.end = before.len();
            chunk.patch = true;
        }
        return Ok(chunks);
    }
    splice_frames(before, after)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    path: Vec<String>,
    value: Option<serde_json::Value>,
    remove: bool,
}

fn diff(
    old: &serde_json::Value,
    new: &serde_json::Value,
    path: &mut Vec<String>,
    edits: &mut Vec<Edit>,
) {
    use serde_json::Value;
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in b {
                path.push(key.clone());
                if let Some(previous) = a.get(key) {
                    diff(previous, value, path, edits);
                } else {
                    edits.push(Edit {
                        path: path.clone(),
                        value: Some(value.clone()),
                        remove: false,
                    });
                }
                path.pop();
            }
            for key in a.keys().filter(|key| !b.contains_key(*key)) {
                let mut path = path.clone();
                path.push(key.clone());
                edits.push(Edit {
                    path,
                    value: None,
                    remove: true,
                });
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for (index, value) in b.iter().enumerate() {
                path.push(index.to_string());
                if let Some(previous) = a.get(index) {
                    diff(previous, value, path, edits);
                } else {
                    edits.push(Edit {
                        path: path.clone(),
                        value: Some(value.clone()),
                        remove: false,
                    });
                }
                path.pop();
            }
            for index in (b.len()..a.len()).rev() {
                let mut path = path.clone();
                path.push(index.to_string());
                edits.push(Edit {
                    path,
                    value: None,
                    remove: true,
                });
            }
        }
        _ => edits.push(Edit {
            path: path.clone(),
            value: Some(new.clone()),
            remove: false,
        }),
    }
}

fn apply_edits(text: &str, data: &str) -> anyhow::Result<String> {
    use serde_json::Value;
    let mut root: Value = serde_json::from_str(text)?;
    let edits: Vec<Edit> = serde_json::from_str(data)?;
    for edit in edits {
        let Some((key, parents)) = edit.path.split_last() else {
            ensure!(!edit.remove, "Cannot remove history root");
            root = edit.value.unwrap_or(Value::Null);
            continue;
        };
        let mut parent = &mut root;
        for segment in parents {
            parent = match parent {
                Value::Array(values) => values.get_mut(segment.parse::<usize>()?),
                Value::Object(values) => values.get_mut(segment),
                _ => None,
            }
            .ok_or_else(|| anyhow::anyhow!("Invalid history patch path"))?;
        }
        match parent {
            Value::Object(values) => {
                if edit.remove {
                    ensure!(values.remove(key).is_some(), "Missing history field");
                } else {
                    values.insert(key.clone(), edit.value.unwrap_or(Value::Null));
                }
            }
            Value::Array(values) => {
                let index = key.parse::<usize>()?;
                if edit.remove {
                    ensure!(index < values.len(), "Invalid history removal");
                    values.remove(index);
                } else if index == values.len() {
                    values.push(edit.value.unwrap_or(Value::Null));
                } else {
                    ensure!(index < values.len(), "Invalid history index");
                    values[index] = edit.value.unwrap_or(Value::Null);
                }
            }
            _ => anyhow::bail!("Invalid history patch parent"),
        }
    }
    let text = serde_json::to_string(&root)?;
    ensure!(
        text.len() <= MAX_HISTORY_BYTES,
        "Remote history exceeds assembly limit"
    );
    Ok(text)
}

fn splice_frames(before: &str, after: &str) -> anyhow::Result<Vec<Frame>> {
    let mut start = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(start) || !after.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = before[start..]
        .bytes()
        .rev()
        .zip(after[start..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(before.len() - suffix)
        || !after.is_char_boundary(after.len() - suffix)
    {
        suffix -= 1;
    }
    let end = before.len() - suffix;
    let replacement = &after[start..after.len() - suffix];
    let base = digest(before);
    let mut result = Vec::new();
    let mut offset = 0;
    loop {
        let mut next = (offset + CHUNK).min(replacement.len());
        while !replacement.is_char_boundary(next) {
            next -= 1;
        }
        result.push(Frame {
            base: base.clone(),
            start,
            end,
            offset,
            data: replacement[offset..next].into(),
            done: next == replacement.len(),
            patch: false,
            metadata: None,
        });
        offset = next;
        if offset == replacement.len() {
            break;
        }
    }
    Ok(result)
}

#[derive(Default)]
pub(crate) struct Assembly {
    text: String,
    pending: Option<(String, usize, usize, bool, String)>,
}
impl Assembly {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    pub(crate) fn apply(&mut self, frame: Frame) -> anyhow::Result<bool> {
        if self.pending.is_none() {
            ensure!(
                frame.offset == 0 && frame.base == digest(&self.text),
                "Remote history base mismatch"
            );
            ensure!(
                frame.start <= frame.end
                    && self.text.is_char_boundary(frame.start)
                    && self.text.is_char_boundary(frame.end),
                "Invalid remote history splice"
            );
            self.pending = Some((
                frame.base.clone(),
                frame.start,
                frame.end,
                frame.patch,
                String::new(),
            ));
        }
        let (base, start, end, patch, data) = self.pending.as_mut().unwrap();
        ensure!(
            *base == frame.base
                && *start == frame.start
                && *end == frame.end
                && *patch == frame.patch
                && data.len() == frame.offset,
            "Out of order remote history chunk"
        );
        ensure!(
            self.text.len() - (*end - *start) + data.len() + frame.data.len() <= MAX_HISTORY_BYTES,
            "Remote history exceeds assembly limit"
        );
        data.push_str(&frame.data);
        if frame.done {
            let (_, start, end, patch, data) = self.pending.take().unwrap();
            if patch {
                self.text = apply_edits(&self.text, &data)?;
            } else {
                self.text.replace_range(start..end, &data);
            }
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_history_retains_large_groups_and_sends_only_new_content() {
        let original = format!(
            "{{\"messages\":[\"{}\"],\"run\":0}}",
            "history 🌍".repeat(30000)
        );
        let updated = original.replace("],\"run\":0", ",\"new work\"],\"run\":1");
        let mut receiver = Assembly::default();
        for frame in frames("", &original).unwrap() {
            receiver.apply(frame).unwrap();
        }
        assert_eq!(receiver.text(), original);
        let changes = frames(&original, &updated).unwrap();
        assert!(serde_json::to_vec(&changes).unwrap().len() < 1024);
        for frame in changes {
            receiver.apply(frame).unwrap();
        }
        assert_eq!(receiver.text(), updated);
    }

    #[test]
    fn incremental_history_handles_utf8_replacement_deletion_and_empty_updates() {
        for (before, after) in [
            ("abc🌍end", "abc🌎end"),
            ("long history", "short"),
            ("same", "same"),
            ("text", ""),
        ] {
            let mut receiver = Assembly::default();
            for frame in frames("", before).unwrap() {
                receiver.apply(frame).unwrap();
            }
            for frame in frames(before, after).unwrap() {
                receiver.apply(frame).unwrap();
            }
            assert_eq!(receiver.text(), after);
        }
    }

    #[test]
    fn incremental_history_rejects_wrong_base_and_out_of_order_chunks() {
        let mut receiver = Assembly::default();
        let wrong = frames("unknown base", "changed").unwrap().remove(0);
        assert!(receiver.apply(wrong).is_err());
        let large = "🌍".repeat(40000);
        let chunks = frames("", &large).unwrap();
        assert!(chunks.len() > 1);
        assert!(receiver.apply(chunks[1].clone()).is_err());
        assert_eq!(receiver.text(), "");
    }

    #[test]
    fn separate_history_changes_do_not_resend_unchanged_middle() {
        let old = serde_json::json!([{"state":"pending"},{"text":"unchanged".repeat(200000)}]);
        let mut new = old.clone();
        new[0]["state"] = serde_json::json!("done");
        new.as_array_mut()
            .unwrap()
            .push(serde_json::json!({"text":"new"}));
        let before = old.to_string();
        let after = new.to_string();
        let mut receiver = Assembly::default();
        for frame in frames("", &before).unwrap() {
            receiver.apply(frame).unwrap();
        }
        let updates = frames(&before, &after).unwrap();
        assert!(serde_json::to_vec(&updates).unwrap().len() < 1024);
        for frame in updates {
            receiver.apply(frame).unwrap();
        }
        assert_eq!(receiver.text(), after);
        for frame in frames(&after, "[]").unwrap() {
            receiver.apply(frame).unwrap();
        }
        assert_eq!(receiver.text(), "[]");
    }

    #[test]
    fn incomplete_update_does_not_replace_visible_history() {
        let mut receiver = Assembly::default();
        for frame in frames("", "visible").unwrap() {
            receiver.apply(frame).unwrap();
        }
        let large = "updated 🌍".repeat(10000);
        let chunks = frames("visible", &large).unwrap();
        assert!(!receiver.apply(chunks[0].clone()).unwrap());
        assert_eq!(receiver.text(), "visible");
        let mut reconnected = Assembly::default();
        for frame in frames("", &large).unwrap() {
            reconnected.apply(frame).unwrap();
        }
        assert_eq!(reconnected.text(), large);
    }

    #[test]
    fn assembly_rejects_invalid_offsets_without_panicking() {
        for (start, end) in [(1, 100), (2, 1), (usize::MAX, usize::MAX)] {
            let mut receiver = Assembly::default();
            let mut frame = frames("", "data").unwrap().remove(0);
            frame.start = start;
            frame.end = end;
            assert!(receiver.apply(frame).is_err());
            assert_eq!(receiver.text(), "");
        }
    }
}

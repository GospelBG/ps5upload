//! Paged `MgmtText` methods (`app.list`, `saves.list`, `shots.list`, `videos.list`).
//!
//! The payload cuts the handler's JSON array to a window that fits one reply
//! (`{"offset":N,"limit":M}` in, `more` out, SPEC.md section 7.3). Callers keep the legacy body:
//! this loops until `more` is clear and returns one document, the first page's object with the
//! array of every page, so a console with 6,000 saves reads exactly like one with 6.

use std::time::Duration;

use anyhow::{anyhow, Result};
use ava1::gen;
use ps5upload_core::mgmt::{Method, MgmtError};
use serde_json::{json, Value};

use super::AvaTransport;

/// A console never lists more entries than this in one call (a loop guard, not a limit anyone meets:
/// a page holds a thousand or more entries).
const MAX_PAGES: usize = 4096;

impl AvaTransport {
    pub(super) async fn paged_text(
        &self,
        console: &str,
        method: Method,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        // The request is the legacy one (`{}` or `{"user_id":N}`); the window rides beside its fields.
        let base: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(body)
                .map_err(|e| anyhow!("{label}: request is not JSON: {e}"))?
        };
        let mut merged: Option<Value> = None;
        let mut offset = 0usize;
        for _ in 0..MAX_PAGES {
            let mut req = base.clone();
            req["offset"] = json!(offset);
            let t = self
                .text_page(console, method, label, &serde_json::to_vec(&req)?, timeout)
                .await?;
            let page: Value = serde_json::from_slice(&t.body)
                .map_err(|e| anyhow!("{label}: a page is not JSON: {e}"))?;
            let more = t.more.unwrap_or(0) != 0;
            let n = merge_page(&mut merged, page, label)?;
            offset += n;
            if !more {
                return Ok(serde_json::to_vec(&merged.expect("a first page"))?);
            }
            if n == 0 {
                return Err(MgmtError {
                    label: label.to_string(),
                    status: gen::ERR_INTERNAL,
                    cause: "reply_paged_empty_page".into(),
                }
                .into());
            }
        }
        Err(MgmtError {
            label: label.to_string(),
            status: gen::ERR_INTERNAL,
            cause: "reply_paged_too_many_pages".into(),
        }
        .into())
    }
}

/// The name of the one array in a page object (`apps`, `saves`, `items`).
fn array_key(page: &Value) -> Option<String> {
    page.as_object()?
        .iter()
        .find(|(_, v)| v.is_array())
        .map(|(k, _)| k.clone())
}

/// Adds `page` to `merged`; returns how many array elements the page held. A page that reached the
/// payload's buffer (`"truncated":true`) marks the whole document truncated.
fn merge_page(merged: &mut Option<Value>, page: Value, label: &str) -> Result<usize> {
    let key = array_key(&page).ok_or_else(|| anyhow!("{label}: a page has no array"))?;
    let truncated = page.get("truncated").and_then(Value::as_bool) == Some(true);
    let n = page[&key].as_array().map_or(0, Vec::len);
    match merged {
        None => *merged = Some(page),
        Some(m) => {
            let mut extra = match page {
                Value::Object(mut o) => o.remove(&key).unwrap_or(Value::Null),
                _ => Value::Null,
            };
            let into = m[&key]
                .as_array_mut()
                .ok_or_else(|| anyhow!("{label}: pages disagree on the array"))?;
            if let Some(a) = extra.as_array_mut() {
                into.append(a);
            }
            if truncated {
                m["truncated"] = json!(true);
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_merge_into_one_document_and_keep_the_truncated_mark() {
        let mut m = None;
        assert_eq!(
            merge_page(&mut m, json!({"saves":[{"a":1},{"a":2}]}), "L").unwrap(),
            2
        );
        assert_eq!(
            merge_page(&mut m, json!({"saves":[{"a":3}],"truncated":true}), "L").unwrap(),
            1
        );
        let m = m.unwrap();
        assert_eq!(m["saves"].as_array().unwrap().len(), 3);
        assert_eq!(m["truncated"], true);
        assert_eq!(m["saves"][2]["a"], 3);
    }

    #[test]
    fn a_page_without_an_array_is_an_error() {
        assert!(merge_page(&mut None, json!({"ok":true}), "L").is_err());
    }
}

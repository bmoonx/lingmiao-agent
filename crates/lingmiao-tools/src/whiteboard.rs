//! Whiteboard tools (Q6) — agent-editable pages shared with the TUI.
//!
//! The whiteboard is a paginated note surface (running summaries, task context,
//! cross-chat hand-off). Page `0` always means "the current page".
//!
//! ## Storage
//!
//! The Python original persisted pages to SQLite (`memory/whiteboard.db`). For
//! M3 the Rust layer keeps a self-contained JSON document at
//! `<root>/.memory/whiteboard/pages.json` (需求③: the memory dir is `.memory/`)
//! — the memory crate has no whiteboard store yet, and this keeps the tool
//! crate dependency-light (decision noted on the whiteboard; revisit if a
//! shared SQLite store lands in lingmiao-memory).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lingmiao_core::brand;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Page {
    title: String,
    content: String,
    created_at: String,
    updated_at: String,
}

impl Page {
    fn new(title: impl Into<String>) -> Self {
        let ts = now_iso();
        Self {
            title: title.into(),
            content: String::new(),
            created_at: ts.clone(),
            updated_at: ts,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WhiteboardData {
    pages: Vec<Page>,
    current_page: usize,
}

impl Default for WhiteboardData {
    fn default() -> Self {
        Self {
            pages: vec![Page::new("Page 1")],
            current_page: 1,
        }
    }
}

/// A lightweight snapshot of one page (for listings).
#[derive(Debug, Clone)]
pub struct PageInfo {
    /// 1-based page number.
    pub page: usize,
    /// Page title.
    pub title: String,
    /// Last-update timestamp.
    pub updated_at: String,
}

/// Persistent, thread-safe whiteboard page store.
pub struct WhiteboardStore {
    path: PathBuf,
    inner: Mutex<WhiteboardData>,
}

impl WhiteboardStore {
    /// Open (or initialise) the whiteboard under `<root>/.memory/whiteboard/`.
    pub fn in_root(root: impl AsRef<Path>) -> Self {
        let path = root
            .as_ref()
            .join(brand::MEMORY_DIR)
            .join("whiteboard")
            .join("pages.json");
        Self::open(path)
    }

    /// Open a whiteboard at an explicit file path.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<WhiteboardData>(&s).ok())
            .filter(|d| !d.pages.is_empty())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(data),
        }
    }

    fn persist(&self, data: &WhiteboardData) -> Result<(), ToolError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ToolError::io(parent.display().to_string(), e))?;
        }
        let json = serde_json::to_string_pretty(data)
            .map_err(|e| ToolError::Other(format!("whiteboard serialise: {e}")))?;
        fs::write(&self.path, json).map_err(|e| ToolError::io(self.path.display().to_string(), e))
    }

    /// List pages (newest metadata first is not needed; ordered by number).
    pub fn list_pages(&self) -> Vec<PageInfo> {
        let data = self.inner.lock().unwrap();
        data.pages
            .iter()
            .enumerate()
            .map(|(i, p)| PageInfo {
                page: i + 1,
                title: p.title.clone(),
                updated_at: p.updated_at.clone(),
            })
            .collect()
    }

    /// Current page number.
    pub fn current_page(&self) -> usize {
        self.inner.lock().unwrap().current_page
    }

    /// Total page count.
    pub fn total_pages(&self) -> usize {
        self.inner.lock().unwrap().pages.len()
    }

    /// Read the effective page number (`0` → current).
    fn effective(&self, data: &WhiteboardData, page: usize) -> usize {
        if page == 0 { data.current_page } else { page }
    }

    /// Read a page. Returns `(page_number, title, content)`.
    pub fn read_page(&self, page: usize) -> Result<(usize, String, String), ToolError> {
        let data = self.inner.lock().unwrap();
        let n = self.effective(&data, page);
        let p = data.pages.get(n.saturating_sub(1)).ok_or_else(|| {
            ToolError::Other(format!("page {n} out of range (1..{})", data.pages.len()))
        })?;
        Ok((n, p.title.clone(), p.content.clone()))
    }

    /// Replace or append content on a page.
    pub fn update_page(
        &self,
        content: &str,
        page: usize,
        append: bool,
    ) -> Result<(usize, String), ToolError> {
        let mut data = self.inner.lock().unwrap();
        let n = self.effective(&data, page);
        let total = data.pages.len();
        if n < 1 || n > total {
            return Err(ToolError::Other(format!(
                "page {n} out of range (1..{total})"
            )));
        }
        let now = now_iso();
        let p = &mut data.pages[n - 1];
        if append {
            let prefix = p.content.trim_end();
            let sep = if prefix.is_empty() { "" } else { "\n\n---\n\n" };
            p.content = format!("{prefix}{sep}{}", content.trim());
        } else {
            p.content = content.to_string();
        }
        p.updated_at = now;
        let title = p.title.clone();
        self.persist(&data)?;
        Ok((n, title))
    }

    /// Create a new page and switch to it.
    pub fn new_page(&self, title: &str) -> Result<(usize, String), ToolError> {
        let mut data = self.inner.lock().unwrap();
        let n = data.pages.len() + 1;
        let page_title = if title.trim().is_empty() {
            format!("Page {n}")
        } else {
            title.trim().to_string()
        };
        data.pages.push(Page::new(page_title.clone()));
        data.current_page = n;
        self.persist(&data)?;
        Ok((n, page_title))
    }

    /// Clear a page's content.
    pub fn clear_page(&self, page: usize) -> Result<(usize, String), ToolError> {
        self.update_page("", page, false)
    }

    /// Delete a page (the last remaining page is just cleared instead).
    pub fn delete_page(&self, page: usize) -> Result<(usize, usize, usize), ToolError> {
        let mut data = self.inner.lock().unwrap();
        let total = data.pages.len();
        if page < 1 || page > total {
            return Err(ToolError::Other(format!(
                "page {page} out of range (1..{total})"
            )));
        }
        if total == 1 {
            data.pages[0].content.clear();
            data.pages[0].updated_at = now_iso();
            self.persist(&data)?;
            return Ok((1, 1, 1));
        }
        data.pages.remove(page - 1);
        let new_total = data.pages.len();
        data.current_page = data.current_page.clamp(1, new_total);
        let current = data.current_page;
        self.persist(&data)?;
        Ok((page, current, new_total))
    }
}

// ── tool args ──────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadPageArgs {
    /// Page number to read. `0` = the current page.
    #[serde(default)]
    page: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdatePageArgs {
    /// Content to write (replaces existing content unless appending).
    content: String,
    /// Page number. `0` = the current page.
    #[serde(default)]
    page: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NewPageArgs {
    /// Optional title; defaults to `Page N`.
    #[serde(default)]
    title: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DeletePageArgs {
    /// Page number to delete.
    page: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct EmptyArgs {}

/// Which whiteboard operation a [`WhiteboardTool`] performs.
#[derive(Debug, Clone, Copy)]
enum WbOp {
    List,
    Read,
    Update,
    Append,
    NewPage,
    Clear,
    Delete,
}

impl WbOp {
    fn name(self) -> &'static str {
        match self {
            WbOp::List => "whiteboard_list",
            WbOp::Read => "whiteboard_read",
            WbOp::Update => "whiteboard_update",
            WbOp::Append => "whiteboard_append",
            WbOp::NewPage => "whiteboard_new_page",
            WbOp::Clear => "whiteboard_clear",
            WbOp::Delete => "whiteboard_delete_page",
        }
    }

    fn description(self) -> &'static str {
        match self {
            WbOp::List => "List all whiteboard pages with titles and update times.",
            WbOp::Read => "Read a whiteboard page (page 0 = current page).",
            WbOp::Update => "Replace the contents of a whiteboard page.",
            WbOp::Append => "Append to a whiteboard page (adds a separator).",
            WbOp::NewPage => "Create a new whiteboard page and switch to it.",
            WbOp::Clear => "Clear a whiteboard page's contents.",
            WbOp::Delete => "Delete a whiteboard page.",
        }
    }

    fn parameters(self) -> Value {
        match self {
            WbOp::List => json_schema::<EmptyArgs>(),
            WbOp::Read | WbOp::Clear => json_schema::<ReadPageArgs>(),
            WbOp::Update | WbOp::Append => json_schema::<UpdatePageArgs>(),
            WbOp::NewPage => json_schema::<NewPageArgs>(),
            WbOp::Delete => json_schema::<DeletePageArgs>(),
        }
    }
}

fn parse<T: for<'de> Deserialize<'de>>(tool: &str, args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::invalid(tool, e.to_string()))
}

/// A single whiteboard tool bound to a shared store.
pub struct WhiteboardTool {
    store: Arc<WhiteboardStore>,
    op: WbOp,
}

#[async_trait]
impl Tool for WhiteboardTool {
    fn name(&self) -> &str {
        self.op.name()
    }
    fn description(&self) -> &str {
        self.op.description()
    }
    fn parameters(&self) -> Value {
        self.op.parameters()
    }
    fn is_mutating(&self) -> bool {
        !matches!(self.op, WbOp::List | WbOp::Read)
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        match self.op {
            WbOp::List => {
                let pages = self.store.list_pages();
                if pages.is_empty() {
                    return Ok(ToolOutput::ok("Whiteboard is empty."));
                }
                let current = self.store.current_page();
                let lines: Vec<String> = pages
                    .iter()
                    .map(|p| {
                        let marker = if p.page == current { "●" } else { "○" };
                        let ts = p.updated_at.get(..19).unwrap_or(&p.updated_at);
                        format!("{marker} Page {}: {} (updated {ts})", p.page, p.title)
                    })
                    .collect();
                Ok(ToolOutput::ok(lines.join("\n")))
            }
            WbOp::Read => {
                let a: ReadPageArgs = parse(self.op.name(), arguments)?;
                match self.store.read_page(a.page) {
                    Ok((n, title, content)) => {
                        let body = if content.trim().is_empty() {
                            "[blank]".to_string()
                        } else {
                            content
                        };
                        Ok(ToolOutput::ok(format!("--- Page {n}: {title} ---\n{body}")))
                    }
                    Err(e) => Ok(ToolOutput::ok(format!("[whiteboard_read] {e}"))),
                }
            }
            WbOp::Update | WbOp::Append => {
                let a: UpdatePageArgs = parse(self.op.name(), arguments)?;
                let append = matches!(self.op, WbOp::Append);
                match self.store.update_page(&a.content, a.page, append) {
                    Ok((n, title)) => Ok(ToolOutput::ok(format!(
                        "{} page {n} ({title}).",
                        if append { "Appended to" } else { "Updated" }
                    ))),
                    Err(e) => Ok(ToolOutput::ok(format!("[{}] {e}", self.op.name()))),
                }
            }
            WbOp::NewPage => {
                let a: NewPageArgs = parse(self.op.name(), arguments)?;
                let (n, title) = self.store.new_page(&a.title)?;
                Ok(ToolOutput::ok(format!(
                    "Created and switched to page {n}: {title}."
                )))
            }
            WbOp::Clear => {
                let a: ReadPageArgs = parse(self.op.name(), arguments)?;
                match self.store.clear_page(a.page) {
                    Ok((n, title)) => Ok(ToolOutput::ok(format!("Cleared page {n} ({title})."))),
                    Err(e) => Ok(ToolOutput::ok(format!("[whiteboard_clear] {e}"))),
                }
            }
            WbOp::Delete => {
                let a: DeletePageArgs = parse(self.op.name(), arguments)?;
                match self.store.delete_page(a.page) {
                    Ok((deleted, current, total)) => Ok(ToolOutput::ok(format!(
                        "Deleted page {deleted}. Now on page {current} of {total}."
                    ))),
                    Err(e) => Ok(ToolOutput::ok(format!("[whiteboard_delete_page] {e}"))),
                }
            }
        }
    }
}

/// The seven whiteboard tool names.
pub const TOOL_NAMES: [&str; 7] = [
    "whiteboard_list",
    "whiteboard_read",
    "whiteboard_update",
    "whiteboard_append",
    "whiteboard_new_page",
    "whiteboard_clear",
    "whiteboard_delete_page",
];

/// Register the whiteboard tools against a store, into `registry`.
pub fn register_all(registry: &mut ToolRegistry, store: Arc<WhiteboardStore>) {
    for op in [
        WbOp::List,
        WbOp::Read,
        WbOp::Update,
        WbOp::Append,
        WbOp::NewPage,
        WbOp::Clear,
        WbOp::Delete,
    ] {
        registry.register(WhiteboardTool {
            store: store.clone(),
            op,
        });
    }
}

/// Convenience: register the whole whiteboard group rooted at `root`.
pub fn register(registry: &mut ToolRegistry, root: impl AsRef<Path>) {
    register_all(registry, Arc::new(WhiteboardStore::in_root(root)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lingmiao-wb-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(tag: &str) -> WhiteboardStore {
        WhiteboardStore::open(temp_root(tag).join("pages.json"))
    }

    #[test]
    fn starts_with_one_page() {
        let s = store("init");
        assert_eq!(s.total_pages(), 1);
        assert_eq!(s.current_page(), 1);
        assert_eq!(s.list_pages()[0].title, "Page 1");
    }

    #[test]
    fn update_append_clear_new_and_delete() {
        let s = store("ops");
        s.update_page("hello", 0, false).unwrap();
        assert_eq!(s.read_page(0).unwrap().2, "hello");
        s.update_page("world", 1, true).unwrap();
        assert!(s.read_page(1).unwrap().2.contains("hello"));
        assert!(s.read_page(1).unwrap().2.contains("world"));
        let (n, _) = s.new_page("Plan").unwrap();
        assert_eq!(n, 2);
        assert_eq!(s.current_page(), 2);
        s.clear_page(2).unwrap();
        assert_eq!(s.read_page(2).unwrap().2, "");
        let (deleted, current, total) = s.delete_page(2).unwrap();
        assert_eq!((deleted, current, total), (2, 1, 1));
    }

    #[test]
    fn persists_across_reopen() {
        let root = temp_root("persist");
        let path = root.join("pages.json");
        {
            let s = WhiteboardStore::open(&path);
            s.update_page("durable", 0, false).unwrap();
        }
        let reopened = WhiteboardStore::open(&path);
        assert_eq!(reopened.read_page(0).unwrap().2, "durable");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn registers_seven_tools_with_schemas() {
        let mut reg = ToolRegistry::new();
        register(&mut reg, temp_root("reg"));
        for name in TOOL_NAMES {
            assert!(reg.contains(name), "missing {name}");
        }
    }
}

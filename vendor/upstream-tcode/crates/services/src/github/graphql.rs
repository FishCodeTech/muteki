use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone)]
pub struct Document {
    pub query: String,
    pub variables: BTreeMap<String, Value>,
}
pub type Variables = BTreeMap<String, (String, Value)>;

pub struct AliasItem {
    pub key: usize,
    pub variables: Variables,
}

/// Callers supply trusted schema text; values are delivered only in the variables map.
pub fn aliases(
    operation: &str,
    name: &str,
    prefix: &str,
    items: &[AliasItem],
    shared: &Variables,
    field: impl Fn(&BTreeMap<String, String>) -> String,
    within: impl Fn(String) -> String,
) -> Option<Document> {
    if items.is_empty() {
        return None;
    }
    let mut declarations = Vec::new();
    let mut variables = BTreeMap::new();
    for (name, (kind, value)) in shared {
        declarations.push(format!("${name}: {kind}"));
        variables.insert(name.clone(), value.clone());
    }
    let fields: Vec<_> = items
        .iter()
        .map(|item| {
            let alias = format!("{prefix}{}", item.key);
            let mut placeholders = BTreeMap::new();
            for (name, (kind, value)) in &item.variables {
                let variable = format!("{alias}_{name}");
                declarations.push(format!("${variable}: {kind}"));
                variables.insert(variable.clone(), value.clone());
                placeholders.insert(name.clone(), format!("${variable}"));
            }
            format!("{alias}: {}", field(&placeholders))
        })
        .collect();
    let parameters = if declarations.is_empty() {
        String::new()
    } else {
        format!("({})", declarations.join(", "))
    };
    Some(Document {
        query: format!(
            "{operation} {name}{parameters} {{\n{}\n}}",
            within(fields.join("\n"))
        ),
        variables,
    })
}

#[derive(Debug)]
pub struct Pages<T> {
    pub pages: Vec<T>,
    pub truncated: bool,
}

pub fn pages<T, E>(
    from: Option<String>,
    max_pages: Option<usize>,
    mut read: impl FnMut(Option<&str>, &[T]) -> Result<T, E>,
    next_cursor: impl Fn(&T) -> Option<String>,
    until: impl Fn(&[T]) -> bool,
) -> Result<Pages<T>, E> {
    let mut pages = Vec::new();
    let mut seen = HashSet::new();
    let mut after = from;
    loop {
        let page = read(after.as_deref(), &pages)?;
        let next = next_cursor(&page);
        pages.push(page);
        let Some(next) = next else {
            return Ok(Pages {
                pages,
                truncated: false,
            });
        };
        if !seen.insert(next.clone())
            || max_pages.is_some_and(|max| pages.len() >= max)
            || until(&pages)
        {
            return Ok(Pages {
                pages,
                truncated: true,
            });
        }
        after = Some(next);
    }
}

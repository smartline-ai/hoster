use std::collections::BTreeMap;

pub struct TemplateVars {
    pub registry: String,
    pub tag: String,
    pub branch: String,
    pub sha: String,
    pub urls: BTreeMap<String, String>,
}

/// Replace `{{var}}` placeholders. Supported: `registry`, `tag`, `branch`,
/// `sha`, and `url.<service>` (only for exposed services). Any other
/// placeholder, or a `url.<service>` that is not exposed, is an error naming
/// the offending token — deploys must fail loudly, never ship a literal
/// `{{...}}` into a container.
pub fn substitute(input: &str, vars: &TemplateVars) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| "unclosed '{{' in template".to_string())?;
        let name = after[..end].trim();
        out.push_str(&resolve(name, vars)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve(name: &str, vars: &TemplateVars) -> Result<String, String> {
    match name {
        "registry" => Ok(vars.registry.clone()),
        "tag" => Ok(vars.tag.clone()),
        "branch" => Ok(vars.branch.clone()),
        "sha" => Ok(vars.sha.clone()),
        _ => {
            if let Some(service) = name.strip_prefix("url.") {
                vars.urls.get(service).cloned().ok_or_else(|| {
                    format!("{{{{url.{service}}}}} refers to {service:?}, which is not an exposed service")
                })
            } else {
                Err(format!("unknown template variable {{{{{name}}}}}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        let mut urls = BTreeMap::new();
        urls.insert("backend".to_string(), "https://backend-b1.dev.example.com".to_string());
        TemplateVars {
            registry: "reg.example.com".to_string(),
            tag: "abc123".to_string(),
            branch: "b1".to_string(),
            sha: "deadbeef".to_string(),
            urls,
        }
    }

    #[test]
    fn substitutes_simple_vars() {
        assert_eq!(substitute("{{registry}}/app:{{tag}}", &vars()).unwrap(), "reg.example.com/app:abc123");
    }

    #[test]
    fn substitutes_branch_and_sha() {
        assert_eq!(substitute("{{branch}}-{{sha}}", &vars()).unwrap(), "b1-deadbeef");
    }

    #[test]
    fn substitutes_url_of_exposed_service() {
        assert_eq!(substitute("{{url.backend}}", &vars()).unwrap(), "https://backend-b1.dev.example.com");
    }

    #[test]
    fn no_placeholders_is_identity() {
        assert_eq!(substitute("postgres://postgres:5432/app", &vars()).unwrap(), "postgres://postgres:5432/app");
    }

    #[test]
    fn unknown_var_errors() {
        let e = substitute("{{nope}}", &vars()).unwrap_err();
        assert!(e.contains("nope"), "got: {e}");
    }

    #[test]
    fn url_of_unexposed_service_errors() {
        let e = substitute("{{url.database}}", &vars()).unwrap_err();
        assert!(e.contains("database"), "got: {e}");
    }
}

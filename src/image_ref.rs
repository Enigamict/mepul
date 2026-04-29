use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub reference: String,
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self> {
        let (name_part, reference) = split_reference(input);
        let (registry, repository) = split_registry_and_repository(name_part);

        if repository.is_empty() {
            bail!("repository is empty");
        }

        Ok(Self {
            registry,
            repository,
            reference,
        })
    }

    pub fn display_reference(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.reference)
    }
}

fn split_reference(input: &str) -> (&str, String) {
    let digest_pos = input.find('@');
    if let Some(pos) = digest_pos {
        let name = &input[..pos];
        let reference = input[pos + 1..].to_string();
        return (name, reference);
    }

    let slash_pos = input.rfind('/');
    let colon_pos = input.rfind(':');

    if let Some(colon) = colon_pos {
        if slash_pos.map_or(true, |slash| colon > slash) {
            let name = &input[..colon];
            let reference = input[colon + 1..].to_string();
            return (name, reference);
        }
    }

    (input, "latest".to_string())
}

fn split_registry_and_repository(name: &str) -> (String, String) {
    let mut parts = name.splitn(2, '/');
    let first = parts.next().unwrap_or_default();
    let remainder = parts.next();

    let is_registry = first.contains('.') || first.contains(':') || first == "localhost";

    if is_registry {
        (first.to_string(), remainder.unwrap_or_default().to_string())
    } else {
        let repository = if remainder.is_some() {
            name.to_string()
        } else {
            format!("library/{name}")
        };
        ("registry-1.docker.io".to_string(), repository)
    }
}

#[cfg(test)]
mod tests {
    use super::ImageReference;

    #[test]
    fn parses_docker_hub_library_image() {
        let image = ImageReference::parse("ubuntu:24.04").unwrap();
        assert_eq!(image.registry, "registry-1.docker.io");
        assert_eq!(image.repository, "library/ubuntu");
        assert_eq!(image.reference, "24.04");
    }

    #[test]
    fn parses_explicit_registry() {
        let image = ImageReference::parse("ghcr.io/example/app:1.0").unwrap();
        assert_eq!(image.registry, "ghcr.io");
        assert_eq!(image.repository, "example/app");
        assert_eq!(image.reference, "1.0");
    }
}

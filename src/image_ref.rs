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
    fn parses_image_references() {
        for test in [
            (
                "docker hub library image with tag",
                "ubuntu:24.04",
                "registry-1.docker.io",
                "library/ubuntu",
                "24.04",
            ),
            (
                "docker hub library image without tag",
                "ubuntu",
                "registry-1.docker.io",
                "library/ubuntu",
                "latest",
            ),
            (
                "explicit registry",
                "ghcr.io/example/app:1.0",
                "ghcr.io",
                "example/app",
                "1.0",
            ),
            (
                "digest reference",
                "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "registry-1.docker.io",
                "library/ubuntu",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                "localhost registry with port",
                "localhost:5000/example/app:dev",
                "localhost:5000",
                "example/app",
                "dev",
            ),
        ] {
            let (name, input, registry, repository, reference) = test;
            let image = ImageReference::parse(input).unwrap();

            assert_eq!(image.registry, registry, "{name}: registry");
            assert_eq!(image.repository, repository, "{name}: repository");
            assert_eq!(image.reference, reference, "{name}: reference");
        }
    }

    #[test]
    fn rejects_empty_repository_for_explicit_registry() {
        let error = ImageReference::parse("ghcr.io").unwrap_err();
        assert!(error.to_string().contains("repository is empty"));
    }
}

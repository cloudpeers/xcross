use crate::metadata::Metadata;
use crate::package::Artifact;
use crate::pom::Pom;
use anyhow::Result;
use pubgrub::error::PubGrubError;
use pubgrub::range::Range;
use pubgrub::report::{DefaultStringReporter, Reporter};
use pubgrub::solver::{Dependencies, DependencyProvider};
use std::borrow::Borrow;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod metadata;
mod package;
mod pom;
mod range;

pub use package::{Package, Version};

pub trait Download {
    fn download(&self, url: &str, dest: &Path) -> Result<()>;
}

pub struct Maven<D: Download> {
    client: D,
    cache_dir: PathBuf,
    repositories: Vec<&'static str>,
}

impl<D: Download> Maven<D> {
    pub fn new(cache_dir: PathBuf, client: D) -> Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_dir,
            client,
            repositories: vec![],
        })
    }

    pub fn add_repository(&mut self, repo: &'static str) {
        self.repositories.push(repo);
    }

    pub fn resolve(&self, package: Package, version: Version) -> Result<Vec<PathBuf>> {
        Ok(pubgrub::solver::resolve(self, package, version)
            .map_err(|err| {
                if let PubGrubError::NoSolution(mut tree) = err {
                    tree.collapse_no_versions();
                    anyhow::anyhow!("{}", DefaultStringReporter::report(&tree))
                } else {
                    anyhow::anyhow!("{:?}", err)
                }
            })?
            .into_iter()
            .filter_map(
                |(package, version)| match self.package(&package, &version) {
                    Ok(path) => {
                        if let Ok(metadata) = self.metadata(&package) {
                            log::info!(
                                "selected {} {} (latest {}) (release {})",
                                package,
                                version,
                                metadata.latest(),
                                metadata.release(),
                            );
                        } else {
                            log::info!("selected {} {}", package, version,);
                        }
                        Some(path)
                    }
                    Err(err) => {
                        log::info!("{}", err);
                        None
                    }
                },
            )
            .collect())
    }

    pub fn package(&self, package: &Package, version: &Version) -> Result<PathBuf> {
        let artifact = Artifact { package, version };
        let pom = self.pom(artifact)?;
        self.artifact(artifact, pom.packaging())
    }

    fn versions(&self, package: &Package, range: &Range<Version>) -> Vec<Version> {
        match self.metadata(package) {
            Ok(metadata) => metadata
                .versions()
                .iter()
                .filter_map(|version| Version::from_str(version).ok())
                .filter(|version| {
                    if let Some(suffix) = version.suffix.as_ref() {
                        if suffix.starts_with("alpha")
                            || suffix.starts_with("beta")
                            || suffix.starts_with("RC")
                            || suffix.starts_with('M')
                        {
                            return false;
                        }
                    }
                    true
                })
                .filter(|version| range.contains(version))
                .rev()
                .collect(),
            Err(err) => {
                log::debug!("failed to get metadata for {}: {}", package, err);
                range.lowest_version().into_iter().collect()
            }
        }
    }

    fn metadata(&self, package: &Package) -> Result<Metadata> {
        let path = self.cache_dir.join(package.file_name());
        if !path.exists() {
            let mut downloaded = false;
            for repo in &self.repositories {
                let url = package.url(repo);
                if self.client.download(&url, &path).is_ok() {
                    downloaded = true;
                    break;
                }
            }
            if !downloaded {
                anyhow::bail!("metadata not found for {}", package);
            }
        }
        let s = std::fs::read_to_string(path)?;
        let metadata =
            quick_xml::de::from_str(&s).map_err(|err| anyhow::anyhow!("{}: {}", err, s))?;
        Ok(metadata)
    }

    fn pom(&self, artifact: Artifact) -> Result<Pom> {
        match self.artifact(artifact, "pom") {
            Ok(path) => {
                let s = std::fs::read_to_string(path)?;
                let pom =
                    quick_xml::de::from_str(&s).map_err(|err| anyhow::anyhow!("{}: {}", err, s))?;
                Ok(pom)
            }
            Err(err) => {
                log::info!("{}", err);
                Ok(Default::default())
            }
        }
    }

    fn artifact(&self, artifact: Artifact, ext: &str) -> Result<PathBuf> {
        let path = self.cache_dir.join(artifact.file_name(ext));
        if !path.exists() {
            log::info!("downloading {}", artifact);
            let mut downloaded = false;
            for repo in &self.repositories {
                let url = artifact.url(repo, ext);
                if self.client.download(&url, &path).is_ok() {
                    downloaded = true;
                    break;
                }
            }
            if !downloaded {
                anyhow::bail!("artifact not found {} {}", artifact, ext);
            }
        }
        Ok(path)
    }
}

impl<D: Download> DependencyProvider<Package, Version> for Maven<D> {
    fn choose_package_version<T: Borrow<Package>, U: Borrow<Range<Version>>>(
        &self,
        potential_packages: impl Iterator<Item = (T, U)>,
    ) -> Result<(T, Option<Version>), Box<dyn Error>> {
        let mut selected: Option<(_, Vec<_>)> = None;
        for (p, r) in potential_packages {
            let versions = self.versions(p.borrow(), r.borrow());
            if let Some((_, v)) = &selected {
                if v.len() < versions.len() {
                    continue;
                }
            }
            let early_exit = versions.len() < 2;
            selected = Some((p, versions));
            if early_exit {
                break;
            }
        }
        let (p, v) = selected.expect("non empty iterator");
        let v = v.into_iter().next();
        //log::debug!("chose {} {:?} (latest {}) (release {})", p.borrow(), v);
        Ok((p, v))
    }

    fn get_dependencies(
        &self,
        package: &Package,
        version: &Version,
    ) -> Result<Dependencies<Package, Version>, Box<dyn Error>> {
        //println!("get dependencies {} {}", package, version);
        let pom = self.pom(Artifact { package, version }).unwrap();
        let deps = pom
            .dependencies()
            .iter()
            .filter(|dep| dep.scope() != Some("test"))
            .map(|dep| (dep.package(), dep.range().unwrap()))
            .collect();
        //log::debug!("{} {} has deps {:?}", package, version, deps);
        Ok(Dependencies::Known(deps))
    }
}

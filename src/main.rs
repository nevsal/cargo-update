extern crate cargo_update;
extern crate tabwriter;
extern crate lazysort;
extern crate regex;
extern crate git2;

use std::io::{Write, stdout, sink};
use std::process::{Command, exit};
use std::collections::BTreeMap;
use std::iter::FromIterator;
use tabwriter::TabWriter;
use lazysort::SortedBy;
use std::fmt::Display;
use git2::Repository;
#[cfg(target_os="windows")]
use std::fs::File;
use regex::Regex;
use std::env;
use std::fs;


fn main() {
    let result = actual_main().err().unwrap_or(0);
    exit(result);
}

fn actual_main() -> Result<(), i32> {
    let opts = cargo_update::Options::parse();

    if cfg!(target_os = "windows") {
        let old_version_r = Regex::new(r"cargo-install-update\.exe-v.+").unwrap();
        for old_version in fs::read_dir(env::current_exe().unwrap().parent().unwrap().canonicalize().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|f| old_version_r.is_match(&f.file_name().into_string().unwrap())) {
            fs::remove_file(old_version.path()).unwrap();
        }
    }

    let crates_file = cargo_update::ops::resolve_crates_file(opts.crates_file.1.clone());
    let http_proxy = cargo_update::ops::find_proxy(&crates_file);
    let configuration = cargo_update::ops::PackageConfig::read(&crates_file.with_file_name(".install_config.toml"))?;
    let mut packages = cargo_update::ops::installed_registry_packages(&crates_file);
    let installed_git_packages = if opts.update_git || (opts.update && opts.install) {
        cargo_update::ops::installed_git_repo_packages(&crates_file)
    } else {
        vec![]
    };

    if !opts.filter.is_empty() {
        packages.retain(|p| configuration.get(&p.name).map(|p_cfg| opts.filter.iter().all(|f| f.matches(p_cfg))).unwrap_or(false));
    }
    match (opts.all, opts.to_update.is_empty()) {
        (true, true) => {}
        (true, false) => {
            for pkg in cargo_update::ops::intersect_packages(&packages, &opts.to_update, opts.install, &installed_git_packages).into_iter() {
                if packages.iter().find(|p| p.name == pkg.name).is_none() {
                    packages.push(pkg);
                }
            }
        }
        (false, true) => {
            if opts.update {
                panic!("No packages to update and neither --list nor --all specified, this should've been caught by option parser \
                        (please report to http://github.com/nabijaczleweli/cargo-update)")
            }
        }
        (false, false) => packages = cargo_update::ops::intersect_packages(&packages, &opts.to_update, opts.install, &installed_git_packages),
    }

    // These are all in the same order and (item => [package names]) maps
    let mut registry_urls = BTreeMap::<_, Vec<_>>::new();
    for package in &packages {
        registry_urls.entry(cargo_update::ops::get_index_url(&crates_file, &package.registry).map_err(|e| {
                    eprintln!("Couldn't get registry for {}: {}.", package.name, e);
                    2
                })?)
            .or_default()
            .push(package.name.clone());
    }
    let registry_urls: Vec<_> = registry_urls.into_iter().collect();

    let registries: Vec<_> = Result::from_iter(registry_urls.iter()
        .map(|((registry_url, _), pkg_names)| {
            cargo_update::ops::get_index_path(&opts.cargo_dir.1, &registry_url[..])
                .map(|path| (path, &pkg_names[..]))
                .map_err(|e| {
                    eprintln!("Couldn't get package repository: {}.", e);
                    2
                })
        }))?;
    let mut registry_repos: Vec<_> = Result::from_iter(registries.iter().map(|(registry, _)| {
        Repository::open(&registry).map_err(|_| {
            eprintln!("Failed to open registry repository at {}.", registry.display());
            2
        })
    }))?;
    for (i, mut registry_repo) in registry_repos.iter_mut().enumerate() {
        cargo_update::ops::update_index(&mut registry_repo,
                                        &(registry_urls[i].0).0,
                                        http_proxy.as_ref().map(String::as_str),
                                        &mut if !opts.quiet {
                                            Box::new(stdout()) as Box<dyn Write>
                                        } else {
                                            Box::new(sink()) as Box<dyn Write>
                                        }).map_err(|e| {
                eprintln!("Failed to update index repository: {}.", e);
                2
            })?;
    }
    let latest_registries: Vec<_> = Result::from_iter(registry_repos.iter().zip(registries.iter()).map(|(registry_repo, (registry, _))| {
        registry_repo.revparse_single("origin/master")
            .map_err(|_| {
                eprintln!("Failed to read master branch of registry repository at {}.", registry.display());
                2
            })
    }))?;

    for package in &mut packages {
        let registry_idx = match registries.iter().position(|(_, pkg_names)| pkg_names.contains(&package.name)) {
            Some(i) => i,
            None => {
                panic!("Couldn't find registry for package {} (please report to http://github.com/nabijaczleweli/cargo-update)",
                       &package.name[..])
            }
        };

        let install_prereleases = configuration.get(&package.name).and_then(|c| c.install_prereleases);
        package.pull_version(&latest_registries[registry_idx].as_commit().unwrap().tree().unwrap(),
                             &registry_repos[registry_idx],
                             install_prereleases);
    }

    if !opts.quiet {
        let mut out = TabWriter::new(stdout());
        writeln!(out, "Package\tInstalled\tLatest\tNeeds update").unwrap();
        for (package, package_target_version, package_install_prereleases) in
            packages.iter()
                .map(|p| {
                    let cfg = configuration.get(&p.name);
                    (p, cfg.as_ref().and_then(|c| c.target_version.as_ref()), cfg.as_ref().and_then(|c| c.install_prereleases))
                })
                .sorted_by(|&(ref lhs, lhstv, lhsip), &(ref rhs, rhstv, rhsip)| {
                    (!lhs.needs_update(lhstv, lhsip), &lhs.name).cmp(&(!rhs.needs_update(rhstv, rhsip), &rhs.name))
                }) {
            write!(out, "{}\t", package.name).unwrap();

            if let Some(ref v) = package.version {
                write!(out, "v{}", v).unwrap();
            } else {
                write!(out, "No").unwrap();
            }

            if let Some(tv) = package_target_version {
                write!(out, "\t{}", tv).unwrap();
            } else if let Some(upd_v) = package.update_to_version() {
                write!(out, "\tv{}", upd_v).unwrap();
                if let Some(alt_v) = package.alternative_version.as_ref() {
                    write!(out, " (v{} available)", alt_v).unwrap();
                }
            } else {
                write!(out, "\tN/A").unwrap();
            }

            writeln!(out,
                     "\t{}",
                     if package.needs_update(package_target_version, package_install_prereleases) {
                         "Yes"
                     } else {
                         "No"
                     })
                .unwrap();
        }
        writeln!(out).unwrap();
        out.flush().unwrap();
    }

    let mut success_global = vec![];
    let mut errored_global = vec![];
    let mut result_global = None;

    if opts.update {
        if !opts.force {
            packages.retain(|p| {
                let cfg = configuration.get(&p.name);
                p.needs_update(cfg.as_ref().and_then(|c| c.target_version.as_ref()),
                               cfg.as_ref().and_then(|c| c.install_prereleases))
            });
        }

        packages.retain(|pkg| pkg.update_to_version().is_some());

        if !packages.is_empty() {
            let (success, errored, result): (Vec<String>, Vec<String>, Option<i32>) = packages.into_iter()
                .map(|package| -> (String, Result<(), i32>) {
                    if !opts.quiet {
                        println!("{} {}",
                                 if package.version.is_some() {
                                     "Updating"
                                 } else {
                                     "Installing"
                                 },
                                 package.name);
                    }

                    if cfg!(target_os = "windows") && package.version.is_some() && package.name == "cargo-update" {
                        save_cargo_update_exec(package.version.as_ref().unwrap());
                    }

                    let registry_name = match registry_urls.iter().find(|(_, pkg_names)| pkg_names.contains(&package.name)) {
                        Some(u) => &(u.0).1,
                        None => {
                            panic!("Couldn't find registry URL for package {} (please report to http://github.com/nabijaczleweli/cargo-update)",
                                   &package.name[..])
                        }
                    };
                    let install_res = if let Some(cfg) = configuration.get(&package.name) {
                            Command::new("cargo")
                                .args(&cfg.cargo_args()[..])
                                .args(if opts.quiet { Some("--quiet") } else { None })
                                .arg("--vers")
                                .arg(if let Some(tv) = cfg.target_version.as_ref() {
                                    tv.to_string()
                                } else {
                                    package.update_to_version().unwrap().to_string()
                                })
                                .arg("--registry")
                                .arg(registry_name.as_ref())
                                .arg(&package.name)
                                .args(&opts.cargo_install_args)
                                .status()
                        } else {
                            Command::new("cargo")
                                .arg("install")
                                .arg("-f")
                                .args(if opts.quiet { Some("--quiet") } else { None })
                                .arg("--vers")
                                .arg(package.update_to_version().unwrap().to_string())
                                .arg("--registry")
                                .arg(registry_name.as_ref())
                                .arg(&package.name)
                                .args(&opts.cargo_install_args)
                                .status()
                        }
                        .unwrap();

                    if !opts.quiet {
                        println!();
                    }
                    if !install_res.success() {
                        if cfg!(target_os = "windows") && package.version.is_some() && package.name == "cargo-update" {
                            restore_cargo_update_exec(package.version.as_ref().unwrap());
                        }

                        (package.name, Err(install_res.code().unwrap_or(-1)))
                    } else {
                        (package.name, Ok(()))
                    }
                })
                .fold((vec![], vec![], None), |(mut s, mut e, r), (pn, p)| match p {
                    Ok(()) => {
                        s.push(pn);
                        (s, e, r)
                    }
                    Err(pr) => {
                        e.push(pn);
                        (s, e, r.or_else(|| Some(pr)))
                    }
                });

            if !opts.quiet {
                println!();
                println!("Updated {} package{}.", success.len(), if success.len() == 1 { "" } else { "s" });
            }
            success_global = success;

            if !errored.is_empty() && result.is_some() {
                eprint!("Failed to update ");
                for (i, e) in errored.iter().enumerate() {
                    if i != 0 {
                        eprint!(", ");
                    }
                    eprint!("{}", e);
                }
                eprintln!(".");
                eprintln!();

                if opts.update_git {
                    errored_global = errored;
                    result_global = result;
                } else {
                    return Err(result.unwrap());
                }
            }
        } else {
            if !opts.quiet {
                println!("No packages need updating.");
            }
        }
    }

    if opts.update_git {
        let mut packages = installed_git_packages;

        if !opts.filter.is_empty() {
            packages.retain(|p| configuration.get(&p.name).map(|p_cfg| opts.filter.iter().all(|f| f.matches(p_cfg))).unwrap_or(false));
        }
        if !opts.all {
            packages.retain(|p| opts.to_update.iter().any(|u| p.name == u.0));
        }

        let git_db_dir = crates_file.with_file_name("git").join("db");
        for package in &mut packages {
            package.pull_version(&opts.temp_dir.1, &git_db_dir, http_proxy.as_ref().map(String::as_str));
        }

        if !opts.quiet {
            let mut out = TabWriter::new(stdout());
            writeln!(out, "Package\tInstalled\tLatest\tNeeds update").unwrap();
            for package in packages.iter()
                .sorted_by(|lhs, rhs| (!lhs.needs_update(), &lhs.name).cmp(&(!rhs.needs_update(), &rhs.name))) {
                writeln!(out,
                         "{}\t{}\t{}\t{}",
                         package.name,
                         package.id,
                         package.newest_id.as_ref().unwrap(),
                         if package.needs_update() { "Yes" } else { "No" })
                    .unwrap();
            }
            writeln!(out).unwrap();
            out.flush().unwrap();
        }

        if opts.update {
            if !opts.force {
                packages.retain(cargo_update::ops::GitRepoPackage::needs_update);
            }

            if !packages.is_empty() {
                let (success, errored, result): (Vec<String>, Vec<String>, Option<i32>) = packages.into_iter()
                    .map(|package| -> (String, Result<(), i32>) {
                        if !opts.quiet {
                            println!("Updating {} from {}", package.name, package.url);
                        }

                        if cfg!(target_os = "windows") && package.name == "cargo-update" {
                            save_cargo_update_exec(&package.id.to_string());
                        }

                        let install_res = if let Some(cfg) = configuration.get(&package.name) {
                                let mut cmd = Command::new("cargo");
                                cmd.args(&cfg.cargo_args()[..])
                                    .args(if opts.quiet { Some("--quiet") } else { None })
                                    .arg("--git")
                                    .arg(&package.url)
                                    .arg(&package.name);
                                if let Some(ref b) = package.branch.as_ref() {
                                    cmd.arg("--branch").arg(b);
                                }
                                cmd.args(&opts.cargo_install_args).status()
                            } else {
                                let mut cmd = Command::new("cargo");
                                cmd.arg("install")
                                    .arg("-f")
                                    .args(if opts.quiet { Some("--quiet") } else { None })
                                    .arg("--git")
                                    .arg(&package.url)
                                    .arg(&package.name);
                                if let Some(ref b) = package.branch.as_ref() {
                                    cmd.arg("--branch").arg(b);
                                }
                                cmd.args(&opts.cargo_install_args).status()
                            }
                            .unwrap();

                        if !opts.quiet {
                            println!();
                        }
                        if !install_res.success() {
                            if cfg!(target_os = "windows") && package.name == "cargo-update" {
                                restore_cargo_update_exec(&package.id.to_string());
                            }

                            (package.name, Err(install_res.code().unwrap_or(-1)))
                        } else {
                            (package.name, Ok(()))
                        }
                    })
                    .fold((vec![], vec![], None), |(mut s, mut e, r), (pn, p)| match p {
                        Ok(()) => {
                            s.push(pn);
                            (s, e, r)
                        }
                        Err(pr) => {
                            e.push(pn);
                            (s, e, r.or_else(|| Some(pr)))
                        }
                    });

                if !opts.quiet {
                    println!();
                    println!("Updated {} git package{}.", success.len(), if success.len() == 1 { "" } else { "s" });
                }
                success_global.extend(success);

                if !errored.is_empty() && result.is_some() {
                    eprint!("Failed to update ");
                    for (i, e) in errored.iter().enumerate() {
                        if i != 0 {
                            eprint!(", ");
                        }
                        eprint!("{}", e);
                    }
                    eprintln!(".");
                    eprintln!();

                    errored_global.extend(errored);

                    if result_global.is_none() {
                        return Err(result.unwrap());
                    }
                }
            } else {
                if !opts.quiet {
                    println!("No git packages need updating.");
                }
            }
        }
    }

    if opts.update {
        if !opts.quiet {
            print!("Overall updated {} package{}",
                   success_global.len(),
                   match success_global.len() {
                       0 => "s",
                       1 => ": ",
                       _ => "s: ",
                   });
            for (i, e) in success_global.iter().enumerate() {
                if i != 0 {
                    print!(", ");
                }
                print!("{}", e);
            }
            println!(".");
        }

        if !errored_global.is_empty() && result_global.is_some() {
            eprint!("Overall failed to update {} package{}",
                    errored_global.len(),
                    match errored_global.len() {
                        0 => "s",
                        1 => ": ",
                        _ => "s: ",
                    });
            for (i, e) in errored_global.iter().enumerate() {
                if i != 0 {
                    eprint!(", ");
                }
                eprint!("{}", e);
            }
            eprintln!(".");

            return Err(result_global.unwrap());
        }
    }

    Ok(())
}


/// This way the past-current exec will be "replaced" and we'll get no dupes in .cargo.toml
#[cfg(target_os="windows")]
fn save_cargo_update_exec<D: Display>(version: &D) {
    save_cargo_update_exec_impl(format!("exe-v{}", version));
}

#[cfg(target_os="windows")]
fn save_cargo_update_exec_impl(extension: String) {
    let cur_exe = env::current_exe().unwrap();
    fs::rename(&cur_exe, cur_exe.with_extension(extension)).unwrap();
    File::create(cur_exe).unwrap();
}

#[cfg(target_os="windows")]
fn restore_cargo_update_exec<D: Display>(version: &D) {
    restore_cargo_update_exec_impl(format!("exe-v{}", version))
}

#[cfg(target_os="windows")]
fn restore_cargo_update_exec_impl(extension: String) {
    let cur_exe = env::current_exe().unwrap();
    fs::remove_file(&cur_exe).unwrap();
    fs::rename(cur_exe.with_extension(extension), cur_exe).unwrap();
}


#[cfg(not(target_os="windows"))]
fn save_cargo_update_exec<D: Display>(_: &D) {}

#[cfg(not(target_os="windows"))]
fn restore_cargo_update_exec<D: Display>(_: &D) {}

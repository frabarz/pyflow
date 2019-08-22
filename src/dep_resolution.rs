use crate::dep_types::DependencyError;
use crate::{
    //    dep_types::{self, Constraint, Dependency, Req, ReqType, Version},
    dep_types::{Constraint, Req, ReqType, Version},
    util,
};
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct WarehouseInfo {
    name: String, // Pulling this ensure proper capitalization
    requires_dist: Option<Vec<String>>,
    requires_python: Option<String>,
    version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WarehouseDigests {
    pub md5: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WarehouseRelease {
    // Could use digests field, which has sha256 as well as md5.
    pub filename: String,
    pub has_sig: bool,
    pub digests: WarehouseDigests,
    pub packagetype: String,
    pub python_version: String,
    pub requires_python: Option<String>,
    pub url: String,
    pub dependencies: Option<Vec<String>>,
}

/// Only deserialize the info we need to resolve dependencies etc.
#[derive(Debug, Deserialize)]
struct WarehouseData {
    info: WarehouseInfo,
    releases: HashMap<String, Vec<WarehouseRelease>>,
    urls: Vec<WarehouseRelease>,
}

/// Fetch data about a package from the Pypi Warehouse.
/// https://warehouse.pypa.io/api-reference/json/
fn get_warehouse_data(name: &str) -> Result<WarehouseData, reqwest::Error> {
    let url = format!("https://pypi.org/pypi/{}/json", name);
    let resp = reqwest::get(&url)?.json()?;
    Ok(resp)
}

/// Find the latest version of a package by querying the warehouse.  Also return
/// a vec of the versions found, so we can reuse this later without fetching a second time.
/// Return name to, so we get correct capitalization.
pub fn get_version_info(name: &str) -> Result<(String, Version, Vec<Version>), DependencyError> {
    println!("(dbg) Getting version info for {}", name);
    let data = get_warehouse_data(name)?;

    let all_versions = data
        .releases
        .keys()
        .filter(|v| Version::from_str(v).is_ok())
        // todo: way to do this in one step, like filter_map?
        .map(|v| Version::from_str(v).unwrap())
        .collect();

    match Version::from_str(&data.info.version) {
        Ok(v) => Ok((data.info.name, v, all_versions)),
        // Unable to parse the version listed in info; iterate through releases.
        Err(_) => Ok((
            data.info.name,
            *all_versions
                .iter()
                .max()
                .expect(&format!("Can't find a valid version for {}", name)),
            all_versions,
        )),
    }
}

/// Get release data from the warehouse, ie the file url, name, and hash.
pub fn get_warehouse_release(
    name: &str,
    version: &Version,
) -> Result<Vec<WarehouseRelease>, reqwest::Error> {
    let data = get_warehouse_data(name)?;

    // If there are 0s in the version, and unable to find one, try 1 and 2 digit versions on Pypi.
    let mut release_data = data.releases.get(&version.to_string2());
    if release_data.is_none() && version.patch == 0 {
        release_data = data.releases.get(&version.to_string_med());
        if release_data.is_none() && version.minor == 0 {
            release_data = data.releases.get(&version.to_string_short());
        }
    }

    let release_data = release_data
        .unwrap_or_else(|| panic!("Unable to find a release for {} = \"{}\"", name, version));

    Ok(release_data.clone())
}

#[derive(Clone, Debug, Deserialize)]
struct ReqCache {
    // Name is present from pydeps if getting deps for multiple package names. Otherwise, we ommit
    // it since we already know the name when making the request.
    name: Option<String>,
    version: String,
    requires_python: Option<String>,
    requires_dist: Vec<String>,
}

impl ReqCache {
    fn reqs(&self) -> Vec<Req> {
        self.requires_dist
            .iter()
            // todo: way to filter ok?
            .filter(|vr| Req::from_str(vr, true).is_ok())
            .map(|vr| Req::from_str(vr, true).unwrap())
            //            .expect("Problem parsing req: ")  // todo how do I do this?
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct MultipleBody {
    // name, (version, version). Having trouble implementing Serialize for Version.
    packages: HashMap<String, Vec<String>>,
}

/// Fetch items from multiple packages; cuts down on API calls.
fn get_req_cache_multiple(
    packages: &HashMap<String, Vec<Version>>,
) -> Result<Vec<ReqCache>, reqwest::Error> {
    // input tuple is name, min version, max version.
    println!("(dbg) Getting pydeps data for {:?}", packages);
    // parse strings here.
    let mut packages2 = HashMap::new();
    for (name, versions) in packages.into_iter() {
        let versions = versions.iter().map(|v| v.to_string2()).collect();
        packages2.insert(name.to_owned(), versions);
    }

    let url = "https://pydeps.herokuapp.com/multiple/";
    //    let url = "http://localhost:8000/multiple/";

    Ok(reqwest::Client::new()
        .post(url)
        .json(&MultipleBody {
            packages: packages2,
        })
        .send()?
        .json()?)
}

//fn flatten(result: &mut Vec<Dependency>, tree: &Dependency) {
//    for node in tree.dependencies.iter() {
//        // We don't need sub-deps in the result; they're extraneous info. We only really care about
//        // the name and version requirements.
//        let mut result_dep = node.clone();
//        result_dep.dependencies = vec![];
//        result.push(result_dep);
//        flatten(result, &node);
//    }
//}

/// Helper fn for `guess_graph`.
fn is_compat(constraints: &[Constraint], vers: &Version) -> bool {
    for constraint in constraints.iter() {
        if !constraint.is_compatible(&vers) {
            return false;
        }
    }
    true
}

/// Pull data on pydeps for a req. Only pull what we need.
/// todo: Group all reqs and pull with a single call to pydeps to improve speed?
fn fetch_req_data(
    reqs: &[&Req],
    vers_cache: &mut HashMap<String, (String, Version, Vec<Version>)>,
) -> Result<Vec<ReqCache>, DependencyError> {
    // Narrow-down our list of versions to query.

    let mut query_data = HashMap::new();
    for req in reqs {
        // todo: cache version info; currently may get this multiple times.
        let (name, latest_version, all_versions) = match vers_cache.get(&req.name) {
            Some(c) => c.clone(),
            None => {
                match get_version_info(&req.name) {
                    Ok(data) => {
                        vers_cache.insert(req.name.clone(), data.clone());
                        data
                    }
                    Err(e) => {
                        util::abort(&format!("Can't get version info for the dependency `{}`. Is it spelled correctly?", &req.name));
                        ("".to_string(), Version::new(0, 0, 0), vec![]) // match-compatibility placeholder
                    }
                }
            }
        };

        // For no constraints, default to only gettinNg the latest
        //        let mut min_v_to_query = latest_version;
        //        let mut max_v_to_query = Version::new(0, 0, 0);
        let mut max_v_to_query = latest_version;

        // Find the maximum version compatible with the constraints.
        // todo: May need to factor in additional constraints here, and put
        // todo in fn signature for things that don't resolve with the optimal soln.
        for constr in req.constraints.iter() {
            // For Ne, we have two ranges; the second one being ones higher than the version specified.
            // For other types, we only have one item in the compatible range.
            let i = match constr.type_ {
                ReqType::Ne => 1,
                _ => 0,
            };

            // Ensure we don't query past the latest.
            max_v_to_query = min(constr.compatible_range()[i].1, max_v_to_query);
        }

        // To minimimize request time, only query the latest compatible version.
        let best_version = match all_versions
            .into_iter()
            .filter(|v| *v <= max_v_to_query)
            .max()
        {
            Some(v) => vec![v],
            None => vec![],
        };

        query_data.insert(req.name.to_owned(), best_version);
    }

    if query_data.is_empty() {
        return Ok(vec![]);
    }
    Ok(get_req_cache_multiple(&query_data)?)
    //    Ok(get_req_cache_single(&req.name, &max_v_to_query)?)
}

// Build a graph: Start by assuming we can pick the newest compatible dependency at each step.
// If unable to resolve this way, subsequently run this with additional deconfliction reqs.
fn guess_graph(
    reqs: &[Req],
    locked: &[(String, Version, Vec<(String, Version)>)],
    os: &crate::Os,
    extras: &[String],
    py_vers: &Version,
    //    result: &mut Vec<(String, Version, Vec<(String, Version)>)>,
    result: &mut Vec<(String, Version, Vec<Req>)>,
    cache: &mut HashMap<(String, Version), Vec<&ReqCache>>,
    vers_cache: &mut HashMap<String, (String, Version, Vec<Version>)>,
    reqs_searched: &mut Vec<Req>,
    //    names_searched: &mut Vec<String>,
) -> Result<(), DependencyError> {
    println!("IN guess graph for {:?}", &reqs);

    let reqs: Vec<&Req> = reqs
        .into_iter()
        // If we've already satisfied this req, don't query it again. Otherwise we'll make extra
        // http calls, and could end up in infinite loops.
        .filter(|r| !reqs_searched.contains(*r))
        //        .filter(|r| !names_searched.contains(&r.name.to_lowercase()))
        .filter(|r| match &r.extra {
            Some(ex) => extras.contains(&ex),
            None => true,
        })
        .filter(|r| match r.sys_platform {
            Some((rt, os_)) => match rt {
                ReqType::Exact => os_ == *os,
                ReqType::Ne => os_ != *os,
                _ => {
                    util::abort("Reqtypes for Os must be == or !=");
                    unreachable!()
                }
            },
            None => true,
        })
        .filter(|r| match &r.python_version {
            Some(v) => v.is_compatible(py_vers),
            None => true,
        })
        .collect();

    // todo: Name checks won't catch imcompat vresion reqs.
    //    for req in reqs.clone().into_iter() {
    //        //        reqs_searched.push(req.clone());
    //        names_searched.push(req.name.clone().to_lowercase());
    //    }

    let mut non_locked_reqs = vec![];
    let mut locked_reqs = vec![];

    // Partition reqs into ones we have lock-file data for, and ones where we need to make
    // http calls to the pypi warehouse (for versions) and pydeps (for deps).
    for req in reqs.iter() {
        reqs_searched.push((*req).clone());

        let mut found_in_locked = false;
        for (name, vers, deps) in locked.iter() {
            if *name != req.name {
                continue;
            }
            if is_compat(&req.constraints, &vers) {
                locked_reqs.push(req.clone());
                found_in_locked = true;
                break;
            }
        }
        if !found_in_locked {
            non_locked_reqs.push(req.clone());
        }
    }

    // Single http call here to pydeps for all this package's reqs, plus version calls for each req.
    let mut query_data = match fetch_req_data(&non_locked_reqs, vers_cache) {
        Ok(d) => d,
        Err(e) => {
            util::abort(&format!("Problem getting dependency data: {:?}", e));
            unreachable!()
        }
    };

    println!("Locked reqs: {:?}", &locked_reqs);

    // Now add info from lock packs for data we didn't query. The purpose of passing locks
    // into the dep resolution process is to avoid unecessary HTTP calls and resolution iterations.
    for req in locked_reqs.iter() {
        // Find the corresponding lock package. There should be exactly one.
        let (lp_name, lp_vers, lp_deps) = locked
            .iter()
            .find(|(name, _, _)| name.to_lowercase() == req.name.to_lowercase())
            .expect("Can't find matching lock package");

        //        let reqs = lp_deps
        //            .iter()
        //            .map(|(name, vers)| {
        //                Req::new(name.clone(), vec![Constraint::new(ReqType::Exact, *vers)])
        //            })
        //            .collect();
        let requires_dist = lp_deps
            .iter()
            .map(|(name, vers)| format!("{} (=={})", name, vers.to_string()))
            .collect();

        // Note that we convert from normal data types to strings here, for the sake of consistency
        // with the http call results.
        query_data.push(ReqCache {
            name: Some(lp_name.clone()),
            version: lp_vers.to_string(),
            requires_python: None,
            requires_dist,
        });
    }

    // We've now merged the query data with locked data. A difference though, is we've already
    // narrowed down the locked ones to one version with an exact constraint.

    for req in reqs {
        // Find matching packages for this requirement.
        let query_result: Vec<&ReqCache> = query_data
            .iter()
            .filter(|d| d.name == Some(req.name.clone()))
            // todo fix filter_compat for modifiers and put back.
            //            .into_iter()
            //            .filter(|r| filter_compat(&req.constraints, &req_cache.version))
            .collect();

        let deps: Vec<(String, Version, Vec<Req>)> = query_result
            .into_iter()
            .map(
                |r| {
                    (
                        req.name.to_owned(),
                        Version::from_str(&r.version).expect("Problem parsing vers"),
                        r.reqs(),
                    )
                }, //                Dependency {
                   //                    name: req.name.to_owned(),
                   //                    version: Version::from_str(&r.version).expect("Problem parsing vers"),
                   //                    reqs: r.reqs(),
                   //                    //                    deps: vec![], // todo
                   //                    constraints_for_this: req.constraints.clone(),
                   //                    extras: vec![], // todo
                   //                }
            )
            .collect();

        if deps.is_empty() {
            util::abort(&format!("Can't find a compatible package for {:?}", &req));
        }

        // Todo: Figure out when newest_compat isn't what you want, due to dealing with
        // todo conflicting sub-reqs.
        let newest_compat = deps
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1))
            .expect("Problem finding newest compatible match");

        result.push(newest_compat.clone());

        if let Err(e) = guess_graph(
            &newest_compat.2,
            locked,
            os,
            extras,
            py_vers,
            result,
            cache,
            vers_cache,
            reqs_searched,
            //            names_searched,
        ) {
            println!("Problem pulling dependency info for {}", &req.name);
            util::abort(&e.details)
        }
    }
    Ok(())
}

/// Determine which dependencies we need to install, using the newest ones which meet
/// all constraints. Gets data from a cached repo, and Pypi. Returns name, version, and name/version of its deps.
//pub fn resolve(tree: &mut DepNode) -> Result<Vec<DepNode>, reqwest::Error> {
pub fn resolve(
    reqs: &[Req],
    locked: &[(String, Version, Vec<(String, Version)>)],
    os: &crate::Os,
    extras: &[String],
    py_vers: &Version,
    //) -> Result<Vec<(String, Version, Vec<Req>)>, reqwest::Error> {
) -> Result<Vec<(String, Version, Vec<(String, Version)>)>, reqwest::Error> {
    let mut result = Vec::new();
    let mut cache = HashMap::new();
    let mut reqs_searched = Vec::new();
    //    let mut names_searched = Vec::new();

    let mut version_cache = HashMap::new();

    if guess_graph(
        &reqs,
        locked,
        os,
        extras,
        py_vers,
        &mut result,
        &mut cache,
        &mut version_cache,
        &mut reqs_searched,
        //        &mut names_searched,
    )
    .is_err()
    {
        util::abort("Problem resolving dependencies");
    }

    let mut by_name: HashMap<String, Vec<(String, Version, Vec<Req>)>> = HashMap::new();

    for dep in result.clone().into_iter() {
        // todo eval if you still need clone later.
        // The formatted name may be different from the pypi one. Eg `IPython` vice `ipython`.
        // todo DRY
        let formatted_name = match &version_cache.get(&dep.0) {
            Some(vc) => vc.0.clone(),
            None => dep.0.clone(), // ie this is from a locked dep.
        };

        match by_name.get_mut(&dep.0) {
            Some(k) => k.push(dep),
            None => {
                by_name.insert(dep.0.clone(), vec![dep]);
            }
        }
    }

    // Deal with duplicates, conflicts etc. The code above assumed no conflicts, and that
    // we can pick the newest compatible version for each req.
    let mut result_cleaned = vec![];
    for (name, deps) in by_name.into_iter() {
        let formatted_name = match &version_cache.get(&name) {
            Some(vc) => vc.0.clone(),
            None => name.clone(), // ie this is from a locked dep.
        };

        if deps.len() == 1 {
            // This dep is only specified once; no need to resolve conflicts.
            let dep = &deps[0];

            // We need to pass subdeps so we can add them to the lock file. Build them from reqs,
            // they each correspond to a resolved dep we'll loop again for.
            let mut subdeps = vec![];
            // todo: Iterate over by_name instead?
            for r in dep.2.iter() {
                for (name, vers, _) in result.iter() {
                    // todo: Make sure you've picked the right one if multiple exist!!
                    // todo qc this. This is what we add to the lockfile deps section.
                    if r.name == *name {
                        subdeps.push((name.to_owned(), *vers));
                    }
                }
            }

            result_cleaned.push((formatted_name.to_owned(), dep.1, subdeps));
        } else {
            // todo: re-impl this! Multiple instances won't lock and install until you do
            //            let constraints: Vec<Vec<Constraint>> = deps
            //                .iter()
            //                .map(|d| d.constraints_for_this.clone())
            //                .collect();
            //
            //            let inter = dep_types::intersection_many(&constraints);
            println!("Specified more than once. name: {}", formatted_name,);
            //            println!("Constr: {:?}", &constraints);

            let newest = deps
                .iter()
                .max_by(|a, b| a.1.cmp(&b.1))
                .expect("Can't find max for newest");

            //            if inter
            //                .iter()
            //                .all(|(min, max)| *min <= newest.1 && newest.1 <= *max)
            //            {

            //                result_cleaned.push((
            //                    formatted_name.to_string(),
            //                    newest.1,
            //                    newest.2.clone(),
            //                ));
            //            } else {
            //                println!("Handle this: intersection doesn't overlap newest")
            //            }
        }
    }

    Ok(result_cleaned)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn warehouse_versions() {
        // Makes API call
        // Assume no new releases since writing this test.
        assert_eq!(
            get_version_info("scinot").unwrap().2.sort(),
            vec![
                Version::new(0, 0, 1),
                Version::new(0, 0, 2),
                Version::new(0, 0, 3),
                Version::new(0, 0, 4),
                Version::new(0, 0, 5),
                Version::new(0, 0, 6),
                Version::new(0, 0, 7),
                Version::new(0, 0, 8),
                Version::new(0, 0, 9),
                Version::new(0, 0, 10),
                Version::new(0, 0, 11),
            ]
            .sort()
        );
    }

    //    #[test]
    //    fn warehouse_deps() {
    //        // Makes API call
    //        let req_part = |name: &str, reqs| {
    //            // To reduce repetition
    //            Req::new(name.to_owned(), version_reqs)
    //        };
    //        let vrnew = |t, ma, mi, p| Constraint::new(t, ma, mi, p);
    //        let vrnew_short = |t, ma, mi| Constraint {
    //            type_: t,
    //            major: ma,
    //            minor: Some(mi),
    //            patch: None,
    //        };
    //        use crate::dep_types::ReqType::{Gte, Lt, Ne};

    //        assert_eq!(
    //            _get_warehouse_dep_data("requests", &Version::new(2, 22, 0)).unwrap(),
    //            vec![
    //                req_part("chardet", vec![vrnew(Lt, 3, 1, 0), vrnew(Gte, 3, 0, 2)]),
    //                req_part("idna", vec![vrnew_short(Lt, 2, 9), vrnew_short(Gte, 2, 5)]),
    //                req_part(
    //                    "urllib3",
    //                    vec![
    //                        vrnew(Ne, 1, 25, 0),
    //                        vrnew(Ne, 1, 25, 1),
    //                        vrnew_short(Lt, 1, 26),
    //                        vrnew(Gte, 1, 21, 1)
    //                    ]
    //                ),
    //                req_part("certifi", vec![vrnew(Gte, 2017, 4, 17)]),
    //                req_part("pyOpenSSL", vec![vrnew_short(Gte, 0, 14)]),
    //                req_part("cryptography", vec![vrnew(Gte, 1, 3, 4)]),
    //                req_part("idna", vec![vrnew(Gte, 2, 0, 0)]),
    //                req_part("PySocks", vec![vrnew(Ne, 1, 5, 7), vrnew(Gte, 1, 5, 6)]),
    //                req_part("win-inet-pton", vec![]),
    //            ]
    //        )

    // todo Add more of these, for variety.
    //    }

    // todo: Make dep-resolver tests, including both simple, conflicting/resolvable, and confliction/unresolvable.
}

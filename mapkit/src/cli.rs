//! mapkit-cli — grab zone data into `.mapbin` collections and inspect them.
//!
//! ```text
//! mapkit-cli grab --name "My zone" [--admin "Dar es Salaam" | --poly zone.geojson]
//!                 [--out assets/collections] [--packdir assets/tanzania-pack]
//!                 [--cache tilecache] [--force-city]
//! mapkit-cli info  <file.mapbin>
//! mapkit-cli list  <dir>
//! mapkit-cli stress <out.mapbin>   build a roads-layer regression bundle
//! ```

use mapkit::boundaries::{point_in_ring, slugify, AdminIndex};
use mapkit::bundle::{Bundle, BundleBuilder, BundleMeta};
use mapkit::grab::{bbox_of, grab, parse_csv, set_selection, GrabOptions, GrabSelection, ZoneSpec};
use std::path::{Path, PathBuf};

fn main() {
    // Load `.env` first so MAPBOX_TOKEN / MAPKIT_* are visible to every
    // subcommand (see `.env.example` at the repo root).
    mapkit::grab::load_dotenv();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_help();
        std::process::exit(1);
    }
    let code = match args[1].as_str() {
        "grab" => cmd_grab(&args[2..]),
        "info" => cmd_info(&args[2..]),
        "list" => cmd_list(&args[2..]),
        "stress" => cmd_stress(&args[2..]),
        _ => {
            print_help();
            1
        }
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "mapkit-cli — grab Mapbox map data into .mapbin collections\n\n\
         commands:\n\
         \x20 grab --name <name> [--admin <query> | --poly <file.geojson>]\n\
         \x20      [--out <dir>] [--packdir <dir>] [--cache <dir>] [--force-city]\n\
         \x20      [--span-cap <metres>] [--min-class <service|residential>]\n\
         \x20      [--road-classes <csv>] [--water-classes <csv>]\n\
         \x20      [--waterway-classes <csv>] [--landcover-classes <csv>]\n\
         \x20      [--no-buildings] [--no-rails] [--no-waterways] [--no-boundary-sub]\n\
         \x20 info <file.mapbin>          print the XML manifest of a bundle\n\
         \x20 list <dir>                  summarise every .mapbin in a directory\n\
         \x20 stress <out.mapbin>        write a roads-layer regression bundle with
         \x20                            intentionally malformed geometry\n\n\
         examples:\n\
         \x20 mapkit-cli grab --name \"Dar es Salaam\" --admin \"region:Dar es Salaam\" --out assets/collections\n\
         \x20 mapkit-cli grab --name \"My drawn zone\" --poly /tmp/zone.geojson --out assets/collections\n\
         \x20 mapkit-cli list assets/collections\n\
         \x20 mapkit-cli stress assets/collections/stress-test.mapbin"
    );
}

fn arg_of(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// stable registry id for an admin pack: its directory name
/// (assets/tanzania-pack → "tanzania-pack") — stored in the bundle so every
/// .mapbin knows which zone registry it came from
fn pack_registry_id(packdir: &str) -> String {
    std::path::Path::new(packdir)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| packdir.to_string())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn cmd_grab(args: &[String]) -> i32 {
    let Some(name) = arg_of(args, "--name") else {
        eprintln!("grab needs --name");
        return 1;
    };
    let out_dir = PathBuf::from(arg_of(args, "--out").unwrap_or_else(|| "assets/collections".into()));
    let packdir = arg_of(args, "--packdir").unwrap_or_else(|| "assets/tanzania-pack".into());
    let cache = arg_of(args, "--cache").unwrap_or_else(|| "tilecache".into());
    let force_city = has_flag(args, "--force-city");
    let span_cap: f64 = arg_of(args, "--span-cap")
        .and_then(|v| v.parse().ok())
        .unwrap_or(80_000.0);
    let min_class = arg_of(args, "--min-class");
    let selection = GrabSelection {
        road_allow: parse_csv(arg_of(args, "--road-classes")),
        water_allow: parse_csv(arg_of(args, "--water-classes")),
        waterway_allow: parse_csv(arg_of(args, "--waterway-classes")),
        landcover_allow: parse_csv(arg_of(args, "--landcover-classes")),
        include_buildings: !has_flag(args, "--no-buildings"),
        include_rails: !has_flag(args, "--no-rails"),
        include_waterways: !has_flag(args, "--no-waterways"),
        include_boundary_sub: !has_flag(args, "--no-boundary-sub"),
    };

    let spec = if let Some(poly) = arg_of(args, "--poly") {
        match load_poly(&poly) {
            Ok(mut rings) => {
                if rings.is_empty() {
                    eprintln!("no usable polygon rings in {poly}");
                    return 1;
                }
                // densify coarse drawn polygons so the boundary clip is smooth
                for r in &mut rings {
                    *r = densify_ring(r, 0.002);
                }
                let bbox = bbox_of(&rings);
                ZoneSpec {
                    name,
                    kind: "Zone".into(),
                    district: String::new(),
                    region: String::new(),
                    // drawn polygons carry no registry link (adm 0)
                    source: String::new(),
                    adm: 0,
                    rings,
                    bbox,
                }
            }
            Err(e) => {
                eprintln!("--poly {poly}: {e}");
                return 1;
            }
        }
    } else if let Some(query) = arg_of(args, "--admin") {
        let bdir = format!("{packdir}/boundaries");
        let Some(admin) = AdminIndex::load(&bdir) else {
            eprintln!("cannot load boundaries from {bdir}");
            return 1;
        };
        let Some(z) = admin.find(&query) else {
            eprintln!("zone '{query}' not found in {bdir}");
            return 1;
        };
        let (district, region) = admin.parents(z);
        ZoneSpec {
            name: name.clone(),
            kind: match z.adm {
                1 => "Region".into(),
                2 => "District".into(),
                _ => "Ward".into(),
            },
            district,
            region,
            source: pack_registry_id(&packdir),
            adm: z.adm,
            rings: z.rings.clone(),
            bbox: z.bbox,
        }
    } else {
        eprintln!("grab needs --admin <query> or --poly <file.geojson>");
        return 1;
    };

    mapkit::grab::set_min_class(min_class);
    set_selection(selection.clone());
    let opts = GrabOptions {
        cache,
        span_cap_m: span_cap,
        force_city,
        min_class: None,
        packdir: Some(packdir),
        selection,
    };
    let t = std::time::Instant::now();
    let bundle = match grab(&spec, &opts) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("grab failed: {e}");
            return 1;
        }
    };

    let slug = slugify(&spec.name);
    std::fs::create_dir_all(&out_dir).ok();
    let path = out_dir.join(format!("{slug}.mapbin"));
    let bytes = match bundle.write_to(&path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // sidecar report for humans
    let _ = std::fs::write(
        out_dir.join(format!("{slug}.report.txt")),
        format!(
            "name: {}\nkind: {}\ndistrict: {}\nregion: {}\ncenter: {:.5}, {:.5}\nspan: {:.0} m\nmode: {}\nintersections: {}\nroads: {}\nbuildings: {}\nrivers: {}\nbytes: {}\ntook: {:.1}s\n",
            bundle.meta.name,
            bundle.meta.kind,
            bundle.meta.district,
            bundle.meta.region,
            bundle.meta.center_lat,
            bundle.meta.center_lon,
            bundle.meta.span_m,
            bundle.meta.mode,
            bundle.meta.n_inters,
            bundle.meta.n_roads,
            bundle.meta.n_buildings,
            bundle.meta.n_rivers,
            bytes,
            t.elapsed().as_secs_f32()
        ),
    );

    println!(
        "wrote {} ({:.1} MB) in {:.1}s",
        path.display(),
        bytes as f32 / 1e6,
        t.elapsed().as_secs_f32()
    );
    0
}

fn cmd_info(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: info <file.mapbin>");
        return 1;
    };
    match Bundle::from_file(Path::new(path)) {
        Ok(b) => {
            println!("{}", b.manifest_xml());
            0
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            1
        }
    }
}

fn cmd_list(args: &[String]) -> i32 {
    let dir = args.first().map(PathBuf::from).unwrap_or_else(|| "assets/collections".into());
    let mut found = 0;
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("cannot read dir {}", dir.display());
        return 1;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.extension().and_then(|e| e.to_str()) != Some("mapbin") {
            continue;
        }
        match Bundle::probe_file(&p) {
            Ok(m) => {
                println!(
                    "{:44} {:18} {:7.1}°,{:8.1}° span {:5.1} km {:9} roads {:8} bld",
                    p.file_name().unwrap_or_default().to_string_lossy(),
                    m.name,
                    m.center_lat,
                    m.center_lon,
                    m.span_m / 1000.0,
                    m.n_roads,
                    m.n_buildings
                );
                found += 1;
            }
            Err(e) => println!("{:44} ERROR: {}", p.display(), e),
        }
    }
    if found == 0 {
        println!("(no .mapbin bundles in {})", dir.display());
    }
    0
}

/// load polygon rings from a GeoJSON file (Feature / FeatureCollection /
/// bare geometry); returns outer rings only
fn load_poly(path: &str) -> Result<Vec<Vec<(f64, f64)>>, String> {
    let txt = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&txt).map_err(|e| format!("json: {e}"))?;
    let mut rings = vec![];
    fn push_geom(v: &serde_json::Value, out: &mut Vec<Vec<(f64, f64)>>) {
        match v["type"].as_str().unwrap_or("") {
            "Polygon" => {
                if let Some(arr) = v["coordinates"].as_array() {
                    if let Some(first) = arr.first() {
                        out.push(ring_of(first));
                    }
                }
            }
            "MultiPolygon" => {
                if let Some(polys) = v["coordinates"].as_array() {
                    for p in polys {
                        if let Some(first) = p.as_array().and_then(|a| a.first()) {
                            out.push(ring_of(first));
                        }
                    }
                }
            }
            "Feature" => push_geom(&v["geometry"], out),
            "FeatureCollection" => {
                if let Some(fs) = v["features"].as_array() {
                    for f in fs {
                        push_geom(f, out);
                    }
                }
            }
            _ => {}
        }
    }
    fn ring_of(ring: &serde_json::Value) -> Vec<(f64, f64)> {
        ring.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let a = p.as_array()?;
                        Some((
                            a.first()?.as_f64()?,
                            a.get(1)?.as_f64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    push_geom(&v, &mut rings);
    rings.retain(|r| r.len() >= 3);
    Ok(rings)
}

/// insert points every ~`step` degrees along a ring so coarse drawn
/// polygons clip cleanly
fn densify_ring(ring: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    if ring.len() < 2 {
        return ring.to_vec();
    }
    let mut out = Vec::with_capacity(ring.len() * 4);
    for i in 0..ring.len() - 1 {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[i + 1];
        let d = ((x1 - x0).hypot(y1 - y0)).max(1e-12);
        let n = (d / step).ceil() as usize;
        for k in 0..n {
            let t = k as f64 / n as f64;
            out.push((x0 + t * (x1 - x0), y0 + t * (y1 - y0)));
        }
    }
    out.push(*ring.last().unwrap());
    let _ = point_in_ring; // keep import used
    out
}

/// `mapkit-cli stress <out.mapbin>` — regression bundle for the engine's
/// road-network loader. Builds a roads layer that contains one of every
/// malformed-geometry class the import gate must reject (plus healthy edges
/// that must survive), so a debug engine run exercises both paths.
///
/// Layout (Dar es Salaam centre):
///   node 0..11 : healthy grid nodes
///   node 12    : only referenced by a self-loop (must vanish)
///   node 13    : isolated (never referenced — must vanish)
///   node 14    : non-finite latitude ("inf")
///   node 15    : NaN longitude
///   node 16/17 : coincident pair (sub-mm apart → same f32 position)
fn cmd_stress(args: &[String]) -> i32 {
    let Some(out) = args.first() else {
        eprintln!("usage: stress <out.mapbin>");
        return 1;
    };

    let (clat, clon) = (-6.7924_f64, 39.2083_f64);

    // healthy grid: 12 nodes, ~120 m spacing (0.001° lon ≈ 111 m, 0.001° lat ≈ 111 m)
    let mut inters: Vec<(f64, f64)> = Vec::new();
    for row in 0..4 {
        for col in 0..3 {
            inters.push((clat + 0.001 * row as f64, clon + 0.001 * col as f64));
        }
    }
    // node 12: referenced only by a self-loop and a bad-geometry edge
    inters.push((clat + 0.004, clon + 0.004));
    // node 13: referenced only by degenerate + bad-geometry edges
    inters.push((clat + 0.004, clon + 0.005));
    // node 14: +inf latitude
    inters.push((f64::INFINITY, clon + 0.006));
    // node 15: NaN longitude
    inters.push((clat + 0.004, f64::NAN));
    // nodes 16, 17: 1e-9° apart → identical f32 position at ~7e6 m from origin
    inters.push((clat + 0.005, clon + 0.005));
    inters.push((clat + 0.005 + 1e-9, clon + 0.005 + 1e-9));

    let mut edges: Vec<(usize, usize, u32, bool, u32, Vec<(f64, f64)>)> = Vec::new();
    // healthy grid edges (row chains + column chains)
    for row in 0..4 {
        for col in 0..2 {
            let a = row * 3 + col;
            edges.push((a, a + 1, 1, false, 50, vec![]));
        }
    }
    for row in 0..3 {
        for col in 0..3 {
            let a = row * 3 + col;
            edges.push((a, a + 3, 1, false, 50, vec![]));
        }
    }
    // healthy edge with interior geometry (curved road)
    edges.push((
        0,
        4,
        1,
        false,
        50,
        vec![(clon + 0.0005, clat + 0.0006), (clon + 0.0008, clat + 0.0009)],
    ));

    let n_healthy = edges.len();

    // --- malformed edges (each class must be rejected by the import gate) ---
    // self-loop: a == b
    edges.push((12, 12, 1, false, 50, vec![]));
    // duplicate pair: same nodes as edge 0 (0-1), different order + geometry
    edges.push((1, 0, 2, true, 30, vec![(clon + 0.0015, clat + 0.0005)]));
    // degenerate: geometry identical to endpoints → zero length
    // (nodes 11 & 13 are distinct; node 13 is otherwise unreferenced, so it
    // must never become an intersection)
    edges.push((
        11,
        13,
        1,
        false,
        50,
        vec![(clon + 0.002, clat + 0.002), (clon + 0.003, clat + 0.003)],
    ));
    // coincident endpoints (16 → 17, same f32 position) and no geometry
    edges.push((16, 17, 1, false, 50, vec![]));
    // non-finite endpoint: 14 has inf latitude
    edges.push((0, 14, 1, false, 50, vec![]));
    // non-finite endpoint: 15 has NaN longitude
    edges.push((1, 15, 1, false, 50, vec![]));
    // non-finite interior geometry point (inf lon)
    edges.push((9, 12, 1, false, 50, vec![(clon + 0.0012, f64::INFINITY)]));
    // out-of-extent geometry: a finite but absurd point ~111 km north
    edges.push((8, 9, 1, false, 50, vec![(clon + 0.0015, clat + 1.0)]));
    // non-finite interior geometry point (NaN lat)
    edges.push((10, 13, 1, false, 50, vec![(clon + 0.003, f64::NAN)]));

    // ---- roads layer text (ego-map-txt-v1) — same writer format as grab.rs --
    let mut s = String::new();
    s.push_str(&format!("{} {}\n", inters.len(), edges.len()));
    for (lat, lon) in &inters {
        // mirror the grabber's {:.7} formatting (writes "inf"/"NaN" verbatim)
        s.push_str(&format!("{:.7} {:.7}\n", lat, lon));
    }
    for (a, b, lanes, oneway, speed, pts) in &edges {
        s.push_str(&format!(
            "{} {} {} {} {} {}",
            a,
            b,
            lanes,
            if *oneway { 1 } else { 0 },
            speed,
            pts.len()
        ));
        for &(lo, la) in pts {
            s.push_str(&format!(" {:.7} {:.7}", la, lo));
        }
        s.push('\n');
    }

    let mut b = BundleBuilder::new(BundleMeta {
        name: "Stress Test".into(),
        kind: "Test".into(),
        district: "-".into(),
        region: "-".into(),
        zone_source: "mapkit-cli-stress".into(),
        zone_adm: 0,
        zone_query: "-".into(),
        center_lat: clat,
        center_lon: clon,
        span_m: 1000.0,
        mode: "city".into(),
        vscale: 1.0,
        terrain_size: 8,
        n_inters: inters.len(),
        n_roads: edges.len(),
        n_buildings: 0,
        n_rivers: 0,
        generator: "mapkit-cli stress".into(),
        created: String::new(),
    });
    b.add_layer(
        mapkit::bundle::LAYER_ROADS,
        "road-graph",
        "ego-map-txt-v1",
        s.into_bytes(),
    );

    let bundle = b.finish();
    match bundle.write_to(Path::new(out)) {
        Ok(n) => {
            println!(
                "stress bundle written: {out} ({n} bytes) — {} nodes, {} edges ({} healthy, {} malformed)",
                inters.len(),
                edges.len(),
                n_healthy,
                edges.len() - n_healthy
            );
            println!(
                "expected engine behaviour: 8 malformed edges rejected with diagnostics, \
                 isolated/self-loop-only/coincident nodes never become intersections"
            );
            0
        }
        Err(e) => {
            eprintln!("{out}: {e}");
            1
        }
    }
}

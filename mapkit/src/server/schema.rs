//! Diesel schema — hand-written to match migrations/0001_init exactly
//! (no diesel CLI needed; the migration SQL above is the source of truth).

diesel::table! {
    zones (id) {
        id -> Integer,
        slug -> Text,
        name -> Text,
        kind -> Text,
        district -> Text,
        region -> Text,
        center_lat -> Double,
        center_lon -> Double,
        span_m -> Double,
        mode -> Text,
        vscale -> Double,
        terrain_size -> Integer,
        n_inters -> Integer,
        n_roads -> Integer,
        n_buildings -> Integer,
        n_rivers -> Integer,
        bytes -> Integer,
        status -> Text,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    tiles (id) {
        id -> Integer,
        z -> Integer,
        x -> Integer,
        y -> Integer,
        source -> Text,
        bytes -> Integer,
        fetched_at -> Text,
    }
}

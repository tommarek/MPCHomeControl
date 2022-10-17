use crate::material;

#[derive(Debug)]
struct InsideZone {
    name: String,
    abbrev: String,
    temp: f64, // [°C]
    volume: f64, // [m^3]
    zone_material: Material,
    heat_capacity: f64, // [J/K]
    zone_boundaries = Boundaries
}

impl InsideZone {
    pub fn new(name: String, abbrev: String, volume: f64) {
        InsideZone {
            name: name,
            abbrev: abbrev,
            volume: volume,
            zone_material: air,
            heat_capacity: volume * air.weight_per_m3 * air.thermal_capacitance,
        }
    }
    pub fn add_wall() {

    }
    pub fn add_horizontal() {

    }
    pub fn add_door_window() {

    }
    pub fn add_open_space() {
        
    }
}

#[derive(Debug)]
struct OutsideZone {
    name: String,
    abbrev: String,
    temp: f64,
}

// outside zones
let outside = OutsideZone {
    name: String::from("outside"),
    abbrev: String::from("out"),
    temp: 0.0,
}
let attic = OutsideZone {
    name: String::from("attic"),
    abbrev: String::from("att"),
    temp: 0.0,
}
let garrage = OutsideZone {
    name: String::from("garrage"),
    abbrev: String::from("gar"),
    temp: 0.0,
}
let ground = OutsideZone {
    name: String::from("ground"),
    abbrev: String::from("g"),
    temp: 5.0,
}

// inside zones
let entrance = InsideZone::new(
    name: "entrance",
    abbrev: "ent",
    volume: 4.585 * 2 * 2.55,
)
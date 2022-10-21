use crate::material;
use crate::model::{HorizontalWall, Wall, Window, OpenSpace, WallTypes};

#[derive(Debug)]
struct InsideZone {
    name: String,
    abbrev: String,
    temp: f64, // [°C]
    volume: f64, // [m^3]
    zone_material: Material,
    heat_capacity: f64, // [J/K]
    zone_boundaries: Boundaries,
}
struct ZoneBoundaries {
    horizontal: Vec<HorizontalWall>,
    walls: Vec<Wall>,
    door_windows: Vec<Window>,
    open_space: Vec<OpenSpace>,
}
impl ZoneBoundaries {
    fn new() -> ZoneBoundaries {
        ZoneBoundaries {
            horizontal: Vec::new(),
            walls: Vec::new(),
            door_windows: Vec::new(),
            open_space: Vec::new(),
        }
    }
}

impl InsideZone {
    fn new(name: String, abbrev: String, volume: f64) -> InsideZone {
        InsideZone {
            name: name,
            abbrev: abbrev,
            volume: volume,
            zone_material: material::air,
            heat_capacity: volume * air.weight_per_m3 * air.thermal_capacitance,
            temp: 20.0, // detaulted to 20°C
            zone_boundaries: ZoneBoundaries::new(),
        }
    }
    fn set_temp(temp: f64) {
        self.temp = temp;
    }
    fn add_boundary(boundary: WallTypes) {
        match boundary {
            WallTypes::HorizontalWall => {
                self.zone_boundaries.horizontal.push(boundary);
            }
            WallTypes::Wall => {
                self.zone_boundaries.walls.push(boundary);
            }
            WallTypes::DoorWindow => {
                self.zone_boundaries.door_windows.push(boundary);
            }
            WallTypes::OpenSpace => {
                self.zone_boundaries.open_space.push(boundary);
            }
        }
    }
}

#[derive(Debug)]
struct OutsideZone {
    name: String,
    abbrev: String,
    temp: f64,
}
impl OutsideZone {
    fn new(name: String, abbrev: String) -> OutsideZone {
        OutsideZone {
            name: name,
            abbrev: abbrev,
            temp: 20.0, // detaulted to 20°C
        }
    }
}

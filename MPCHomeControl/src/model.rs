#[derive(Debug)]
struct DoorWindow { // PKS window Uw = 0.72-0.96
    surface_area: f64,
    glass_surface_area: f64,
    u: f64, // heat transfer coef [W/m^2.K] 
    g: f64, // solar g-factor - energy transmittance
}

#[derive(Debug)]
struct OpenSpace { 
    u: f64 // heat transfer coef [W/m^2.K] 
    surface_area: f64,
}

#[derive(Debug)]
struct WallLayer<T> {
    material: T,
    thickness: f64, // [m]
}
#[derive(Debug)]
struct WallType {
    layers: Vec<WallLayer>
    surface_area: f64, // [m^2]
}
#[derive(Debug)]
struct Wall {
    wall_type: WallType,
    t1: f64, // [°C]
    t2: f64, // [°C]
    q1: f64, // [W]
    q2: f64, // [W]
    u1: f64 // heat transfer coef [W/m^2.K]
    u2: f64 // heat transfer coef [W/m^2.K]
}

#[derive(Debug)]
struct HorizontalWall { // floorm cailing
    outer_zone: Zone,
    outer_layers: Vec<WallLayer>,
    inner_zone: Zone,
    inner_layers: Vec<WallLayer>,
    surface_area: f64, // [m^2]
    t1: f64, // [°C]
    t2: f64, // [°C]
    t3: f64, // [°C]
    q1: f64, // [W] floor heating
}

struct Boundaries {
    horizontal: Vec<HorizontalWall>,
    walls: Vec<Wall>,
    door_windows: Vec<Window>,
    open_space: Vec<OpenSpace>,
}


// doors
let entrance_door = DoorWindow{
    u: 0.83, // heat transfer coef [W/m^2.K]
    g: 0.52, // solar g-factor - energy transmittance
    surface_area: 2.42, // [m^2]
}

// windows
let entrance_window = DoorWindow{
    u: 0.74, // heat transfer coef [W/m^2.K]
    g: 0.5, // solar g-factor - energy transmittance
    surface_area: 2.42, // [m^2]
}
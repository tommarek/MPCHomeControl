y#[derive(Debug)]
struct Material {
    name: String,
    thermal_conductivity: f64, // [W/(m.K)]
    thermal_capacitance: f64, // [J/(kg.K)]
    weight_per_m3: f64, // [kg]
}

// materials
let air = Material {
    name: "Air",
    thermal_conductivity: 0.025, // [W/(m.K)]
    thermal_capacitance: 700, // [J/(kg.K)]
    weight_per_m3: 1.199, // [kg]
}
let brick_440 = Material { // Heluz Family 2v1 440
    name: "Exterior wall",
    thermal_conductivity: 0.061, // [W/(m.K)]
    thermal_capacitance: 1000.0, // [J/(kg.K)]
    weight_per_m3: 660, // [kg]
}
let ext_plaster = Material {
    name: "Exterior plaster",
    thermal_conductivity: 0.13, // [W/(m.K)]
    thermal_capacitance: 1000.0, // [J/(kg.K)] no data found about this
    weight_per_m3: 550, // [kg]
}
let int_plaster = Material { // Baumit Ratio L 
    name: "Interior plaster",
    thermal_conductivity: 0.3, // [W/(m.K)]
    thermal_capacitance: 1000.0, // [J/(kg.K)] no data found about this
    weight_per_m3: 800, // [kg]
}
let floor_insulation = Material { // Isover EPS 150
    name: "Floor insulation",
    thermal_conductivity: 0.035, // [W/(m.K)]
    thermal_capacitance: 1270.0, // [J/(kg.K)]
    weight_per_m3: 30, // [kg]
}
let anhydrite = Material { // Isover EPS 150
    name: "Anhydrite",
    thermal_conductivity: 1.8, // [W/(m.K)]
    thermal_capacitance: 1550.0, // [J/(kg.K)] 
    weight_per_m3: 2050, // [kg] 2000-2100
}
let foundation = Material { // Concrete C 20/25
    name: "Foundation concrete",
    thermal_conductivity: 1.5, // [W/(m.K)] 1.23-1774
    thermal_capacitance: 1020.0, // [J/(kg.K)] 
    weight_per_m3: 2250, // [kg] 2200-2300
}

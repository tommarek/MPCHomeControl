#[derive(Debug)]
struct Material {
    name: String,
    thermal_conductivity: f64, // [W/(m.K)]
    specific_heat_capacity: f64, // [J/(kg.K)]
    weight_per_m3: f64, // [kg]
}
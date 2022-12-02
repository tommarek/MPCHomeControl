extern crate nalgebra as na;

use chrono::{DateTime, Datelike, Timelike, Utc};
use na::{Dot, Norm, Vector3};
use uom::si::{
    angle::{degree, radian},
    area::square_meter,
    f64::{Angle, Area, HeatFluxDensity, Length, Pressure, Ratio, TemperatureInterval, Time},
    heat_flux_density::watt_per_square_meter,
    length::centimeter,
    pressure::pascal,
    ratio::{percent, ratio},
    temperature_interval::kelvin,
    time::minute,
};

const SOLAR_CONST: f64 = 1367.0; // W/m^2

// cloud model constants
const CLEAR_SKY_MOLECULAR_SCATTERING: f64 = 0.0685;
const LOW_CLOUDS_TRANSMITTANCE: f64 = 0.36;
const MEDIUM_CLOUDS_TRANSMITTANCE: f64 = 0.38;
const HIGH_CLOUDS_TRANSMITTANCE: f64 = 0.38;
const LOW_CLOUDS_ALBEDO: f64 = 0.36;
const MEDIUM_CLOUDS_ALBEDO: f64 = 0.38;
const HIGH_CLOUDS_ALBEDO: f64 = 0.38;

/// Get three dimensional Vector from azimuth and zenith angle.
/// This can be used to get the norm vector of a surface or the Sun.
///
/// Output vector coordinate system is following:
/// - north -> positive x axis / south -> negative x axis
/// - east -> positive y axis / west -> negative y axis
/// - z: up
///
/// # Arguments
/// * `azimuth` - angle between north and the wall normal / sun vector
/// * `zenith_angle` - angle between the Sun and vertical axis
///
/// # Returns
/// * `Vector3<f64>` - three dimensional vector
pub fn get_vector_from_azimuth_zenith(azimuth: &Angle, zenith_angle: &Angle) -> Vector3<f64> {
    let x = azimuth.cos().get::<ratio>() * zenith_angle.sin().get::<ratio>();
    let y = azimuth.sin().get::<ratio>() * zenith_angle.sin().get::<ratio>();
    let z = zenith_angle.cos().get::<ratio>();
    Vector3::new(x, y, z).normalize()
}

/// Get three dimensional Vector from azimuth and elevation angle.
/// This can be used to get the norm vector of a surface or the Sun.
///
/// Output vector coordinate system is following:
/// - north -> positive x axis / south -> negative x axis
/// - east -> positive y axis / west -> negative y axis
/// - z: up
///
/// # Arguments
/// * `azimuth` - angle between north and the wall normal / sun vector
/// * `elevation_angle` - angle between the surface and horizontal axis
///
/// # Returns
/// * `Vector3<f64>` - three dimensional vector
pub fn get_vector_from_azimuth_elevation(azimuth: &Angle, elevation_angle: &Angle) -> Vector3<f64> {
    let x = azimuth.cos().get::<ratio>() * elevation_angle.cos().get::<ratio>();
    let y = azimuth.sin().get::<ratio>() * elevation_angle.cos().get::<ratio>();
    let z = elevation_angle.sin().get::<ratio>();
    Vector3::new(x, y, z).normalize()
}

// Get a dot product of the surface normal and the sun vector
//
// # Arguments
// * `surface_azimuth` - orientation of the surface
// * `surface_angle` - angle between the surface and horizontal plane
// * `solar_azimuth` - azimuth of the sun
// * `solar_zenith` - zenith angle of the sun (angle between the sun vector and the z axis)
//
// # Returns
// * `f64` - dot product of the surface normal and the sun vector
pub fn get_projection(
    surface_azimuth: &Angle,
    surface_angle: &Angle,
    solar_azimuth: &Angle,
    solar_zenith: &Angle,
) -> f64 {
    let sun_vector = get_vector_from_azimuth_zenith(solar_azimuth, solar_zenith);
    let surface_vector = get_vector_from_azimuth_elevation(surface_azimuth, surface_angle);
    sun_vector.dot(&surface_vector).max(0.0)
}

/// Get a coefficient value used for calculating effectively illuminated area.
///
/// # Arguments
/// * `sun_vector` - normalzied vector pointing to the sun
/// * `surface_normal` - normalized vector of a wall normal
/// # Returns
/// * `f64` - coefficient value [0-1]
fn get_illumination_coefficient(sun_vector: &Vector3<f64>, surface_normal: &Vector3<f64>) -> f64 {
    sun_vector.dot(surface_normal).max(0.0)
}

/// Get the effective illuminated area of a surface. This will be later on used to calculate the
/// solar energy gain of a wall, window, etc.
///
/// # Arguments
/// * `lat` - latitude of the location
/// * `lon` - longitude of the location
/// * `surface_normal` - vector of the surface normal
/// * `surface_area` - area of the surface
/// * `utc` - UTC time
///
/// # Returns
/// * `Area` - effective illuminated area
pub fn get_effective_illuminated_area(
    lat: f64,
    lon: f64,
    surface_normal: &Vector3<f64>,
    surface_area: &Area,
    utc: &DateTime<Utc>,
) -> anyhow::Result<Area> {
    let solar_position = spa::calc_solar_position(*utc, lat, lon)?;
    let sun_vector = get_vector_from_azimuth_zenith(
        &Angle::new::<degree>(solar_position.azimuth),
        &Angle::new::<degree>(solar_position.zenith_angle),
    );
    let surface_normal = surface_normal.normalize();

    let cos_theta = get_illumination_coefficient(&sun_vector, &surface_normal);
    let area: Area = Area::new::<square_meter>(surface_area.get::<square_meter>() * cos_theta);
    anyhow::Ok(area)
}

// Returns typical ground albido value for a given month
// Data can be taken from https://mynasadata.larc.nasa.gov/EarthSystemLAS/UI.vm
// TODO: get data from the API??
//
// # Arguments
// * `utc` - UTC time
//
// # Returns
// * `f64` - ground albido value
pub fn get_typical_albedo(utc: &DateTime<Utc>) -> f64 {
    let month = utc.month();
    match month {
        1 => 0.2333594,
        2 => 0.2644730,
        3 => 0.1343971,
        4 => 0.1407924,
        5 => 0.1643277,
        6 => 0.1625788,
        7 => 0.1502567,
        8 => 0.1511377,
        9 => 0.1553714,
        10 => 0.1376181,
        11 => 0.1215427,
        12 => 0.2467980,
        _ => 0.2,
    }
}

// Estimates total precipitable water from the air temperature and humidity
//
// # Arguments
// * `air_temperature` - air temperature
// * `relative_humidity` - relative humidity
//
// # Returns
// * `Length` - total precipitable water column
pub fn get_total_precipitable_water(
    air_temperature: &TemperatureInterval,
    relative_humidity: &Ratio,
) -> Length {
    let theta = air_temperature.get::<kelvin>() / 273.15;
    let pw = 0.1
        * (0.4976 + 1.5265 * theta + (13.6897 * theta - 14.9188 * (theta).powf(3.0)).exp())
        * (216.7 * relative_humidity.get::<percent>() / (100.0 * air_temperature.get::<kelvin>())
            * (22.330
                - 49.140 * (100.0 / air_temperature.get::<kelvin>())
                - 10.922 * (100.0 / air_temperature.get::<kelvin>()).powf(2.0)
                - 0.39015 * air_temperature.get::<kelvin>() / 100.0)
                .exp());
    Length::new::<centimeter>(pw.max(0.1))
}

// Calculate extraterrestrial solar radiation at any given time.
// Average value for a day is 1367 W/m^2 which is corrected by a distance between the earth and the sun
// Source: http://solardat.uoregon.edu/SolarRadiationBasics.html#Ref3
//
// # Arguments
// * `utc` - UTC time
//
// # Returns
// * `f64` - extraterrestrial solar radiation
pub fn get_extraterrestrial_radiation(utc: &DateTime<Utc>) -> HeatFluxDensity {
    let day_angle: Angle = Angle::new::<degree>(
        (2.0 * std::f64::consts::PI / 365.0) * (f64::from(utc.ordinal()) - 1.0),
    );
    // (R_avg / R) -- R_av is the mean sun-earth distance annd R is the actual sun-earth distance
    let distances_ratio = 1.00011
        + 0.034221 * day_angle.cos().get::<ratio>()
        + 0.00128 * day_angle.sin().get::<ratio>()
        + 0.000719 * (2.0 * day_angle).cos().get::<ratio>()
        + 7.7e-05 * (2.0 * day_angle).sin().get::<ratio>();

    HeatFluxDensity::new::<watt_per_square_meter>(SOLAR_CONST * distances_ratio)
}

#[derive(Debug, Clone, Copy)]
pub struct Irradiance {
    pub diffuse_horizontal_irradiance: HeatFluxDensity,
    pub direct_normal_irradiance: HeatFluxDensity,
    pub global_horizontal_irradiance: HeatFluxDensity,
}
impl Irradiance {
    // Using Reindl model to calculate sky diffuse irradiance on a tilted surface
    // https://strathprints.strath.ac.uk/5008/1/Strachan_PA_et_al_Pure_Empirical_validation_of_models_to_compute_solar_irradiance_on_incsined_surfaces_for_building_energy_simulation_2007.pdf
    // Loutzenhiser P.G. et. al. "Empirical validation of models to compute solar irradiance on incsined surfaces for building energy simulation" 2007, Solar Energy vol. 81. pp. 254-267.
    //
    // # Arguments
    // * `surface_azimuth` - surface azimuth angle
    // * `surface_angle` - angle between the surface and horizontal plane
    //
    // # Returns
    // * `HeatFluxDensity` - sky diffuse irradiance on a tilted surface
    fn get_sky_diffuse_irradiance_on_tilted_surface(
        &self,
        utc: &DateTime<Utc>,
        surface_azimuth: &Angle,
        surface_angle: &Angle,
        solar_azimuth: &Angle,
        solar_zenith: &Angle,
    ) -> HeatFluxDensity {
        let cos_tt = get_projection(surface_azimuth, surface_angle, solar_azimuth, solar_zenith);
        let cos_sol_zenith = solar_zenith.cos().get::<ratio>();

        // Ratio of tilted and horizontal beam irradiance
        let rb = cos_tt / cos_sol_zenith;

        // Anisotropy Index
        let ai = self.direct_normal_irradiance.get::<watt_per_square_meter>()
            / get_extraterrestrial_radiation(utc).get::<watt_per_square_meter>();

        // DNI projected onto horizontal plane
        let hb = (self.direct_normal_irradiance.get::<watt_per_square_meter>() * rb).max(0.0);

        let term1 = 1.0 - ai;
        let term2 = 0.5 * (1.0 + surface_angle.sin().get::<ratio>());
        let term3 = 1.0
            + (hb
                / self
                    .global_horizontal_irradiance
                    .get::<watt_per_square_meter>())
            .sqrt()
                * (0.5 * surface_angle.get::<degree>()).sin().powf(3.0);

        let sky_diffuse = self
            .diffuse_horizontal_irradiance
            .get::<watt_per_square_meter>()
            * (ai * rb + term1 * term2 * term3);

        HeatFluxDensity::new::<watt_per_square_meter>(sky_diffuse.max(0.0))
    }

    // Using Reindl model to calculate ground diffuse irradiance on a tilted surface.
    // The calculation is the last term of equations 3, 4, 7, 8, 10, 11, and 12 in
    // Strachan_PA_et_al_Pure_Empirical_validation_of_models_to_compute_solar_irradiance_on_incsined_surfaces_for_building_energy_simulation_2007
    //
    // # Arguments
    // * `surface_angle` - surface azimuth angle
    // * `albedo` - whiteness of the ground surface
    //
    // # Returns
    // * `HeatFluxDensity` - ground diffuse irradiance on a tilted surface
    fn get_ground_diffuse_irradiance_on_tilted_surface(
        &self,
        surface_angle: &Angle,
        albedo: f64,
    ) -> HeatFluxDensity {
        let diffuse_irrad = 0.5
            * albedo
            * self
                .diffuse_horizontal_irradiance
                .get::<watt_per_square_meter>()
            * (1.0 - surface_angle.cos().get::<ratio>());
        HeatFluxDensity::new::<watt_per_square_meter>(diffuse_irrad.max(0.0))
    }

    /// Get the effective illuminated area of a surface. This will be later on used to calculate the
    /// solar energy gain of a wall, window, etc. This method uses model from
    /// Reindl, D.T., Beckmann, W.A., Duffie, J.A., 1990b. Evaluation of hourly tilted surface radiation models.
    ///
    /// # Arguments
    /// * `surface_azimuth` - surface azimuth angle
    /// * `surface_angle` - angle between the surface and horizontal plane
    ///
    /// # Returns
    /// * `HeatFluxDensity` - total irradiance on a tilted surface
    pub fn get_total_irradiance_on_tilted_surface(
        &self,
        utc: &DateTime<Utc>,
        albedo: f64,
        surface_azimuth: &Angle,
        surface_angle: &Angle,
        solar_azimuth: &Angle,
        solar_zenith: &Angle,
    ) -> HeatFluxDensity {
        let sky_diffuse: HeatFluxDensity = self.get_sky_diffuse_irradiance_on_tilted_surface(
            utc,
            surface_azimuth,
            surface_angle,
            solar_azimuth,
            solar_zenith,
        );
        let ground_diffuse: HeatFluxDensity =
            self.get_ground_diffuse_irradiance_on_tilted_surface(surface_angle, albedo);
        let diffuse = sky_diffuse + ground_diffuse;

        let direct = HeatFluxDensity::new::<watt_per_square_meter>(
            self.direct_normal_irradiance.get::<watt_per_square_meter>()
                * get_projection(surface_azimuth, surface_angle, solar_azimuth, solar_zenith),
        );

        let total = direct + diffuse;

        HeatFluxDensity::new::<watt_per_square_meter>(total.get::<watt_per_square_meter>().max(0.0))
    }
}

#[derive(Debug)]
pub struct ClearSkyIrradiance {
    pub irradiance: Irradiance,
    pub latitude: f64,
    pub longitude: f64,
    pub utc: DateTime<Utc>,
    pub albedo: f64,
    pub solar_zenith: Angle,
    pub solar_azimuth: Angle,
}
impl ClearSkyIrradiance {
    // Calculate clear sky irradiance for a given location and time. This method uses the
    // Bird-Hulstrom clear sky model.
    //
    // # Arguments
    // * `lat` - latitude of the location
    // * `lon` - longitude of the location
    // * `utc` - UTC time
    // * `aod380` - Aerosol optical depth measured at 380nm. Typically from 0.1 to 0.5cm
    // * `aod500` - Aerosol optical depth measured at 500nm. Typically from 0.02 to 0.5cm.
    //              Values > 0.5 represent clouds, volcanic ash, etc.
    // * `precipitable_water` - Total column water vapor. Typically from 0.01 to 6.5cm
    // * `ozone` - Ozone height
    // * `pressure` - Surface pressure
    // * `asymetry` - This factor prescribes what proportion of scattered radiation is sent
    //                off in the same direction as the incoming radiation ("forward scattering").
    //                Bird recommends a value of 0.85 for rural
    // * `albedo` - ground albedo
    //
    // # Returns
    // * `ClearSkyIrradiance` - clear sky irradiance for given location and time
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        utc: &DateTime<Utc>,
        lat: f64,
        lon: f64,
        aod380: &Length,
        aod500: &Length,
        precipitable_water: &Length,
        ozone: &Length,
        pressure: &Pressure,
        asymmetry: &f64,
        albedo: &f64,
    ) -> ClearSkyIrradiance {
        // calculate extraterrestrial radiation
        let dni_extra: HeatFluxDensity = get_extraterrestrial_radiation(utc);

        // get zenith angle
        let solar_position = spa::calc_solar_position(*utc, lat, lon).unwrap();
        let zenith: Angle = Angle::new::<degree>(solar_position.zenith_angle);
        let azimuth: Angle = Angle::new::<degree>(solar_position.azimuth);

        // calculate air mass and pressure corrected air mass
        let airmass = 1.0
            / (zenith.cos().get::<ratio>() + 0.15 * (93.885 - zenith.get::<degree>()).powf(-1.25));
        let am_press = airmass * pressure.get::<pascal>() / 101325.0;

        // rayleigh scattering
        let t_rayleigh =
            (-0.0903 * am_press.powf(0.84) * (1.0 + am_press - am_press.powf(1.01))).exp();

        // ozone absorption
        let am_o3 = airmass * ozone.get::<centimeter>();
        let t_ozone = 1.0
            - 0.1611 * am_o3 * (1.0 + 139.48 * am_o3).powf(-0.3034)
            - 0.002715 * am_o3 / (1.0 + 0.044 * am_o3 + 0.0003 * am_o3.powf(2.0));

        // gasses absorption
        let t_gases = (-0.0127 * am_press.powf(0.26)).exp();

        // water vapor absorption
        let am_h2o = airmass * precipitable_water.get::<centimeter>();
        let t_water =
            1.0 - 2.4959 * am_h2o / ((1.0 + 79.034 * am_h2o).powf(0.6828) + 6.385 * am_h2o);

        // aerosol absorption
        let bird_huldstrom =
            0.27583 * aod380.get::<centimeter>() + 0.35 * aod500.get::<centimeter>();
        let t_aerosol = (-(bird_huldstrom.powf(0.873))
            * (1.0 + bird_huldstrom - bird_huldstrom.powf(0.7088))
            * airmass.powf(0.9108))
        .exp();
        let taa = 1.0 - 0.1 * (1.0 - airmass + airmass.powf(1.06)) * (1.0 - t_aerosol);
        let rs = 0.0685 + (1.0 - asymmetry) * (1.0 - t_aerosol / taa);

        // direct normal irradiance
        let direct_normal_irradiance = 0.9662
            * dni_extra.get::<watt_per_square_meter>()
            * t_aerosol
            * t_water
            * t_gases
            * t_ozone
            * t_rayleigh;

        // direct_horizontal_irradiance
        let ze_cos = if zenith.get::<degree>() < 90.0 {
            zenith.cos().get::<ratio>()
        } else {
            0.0
        };
        let direct_horizontal_irradiance = direct_normal_irradiance * ze_cos;

        // global horizontal irradiance
        let ias = dni_extra.get::<watt_per_square_meter>()
            * ze_cos
            * 0.79
            * t_ozone
            * t_gases
            * t_water
            * taa
            * (0.5 * (1.0 - t_rayleigh) + asymmetry * (1.0 - (t_aerosol / taa)))
            / (1.0 - airmass + airmass.powf(1.02));
        let global_horizontal_irradiance =
            (direct_horizontal_irradiance + ias) / (1.0 - albedo * rs);

        // diffuse horizontal irradiance
        let diffuse_horizontal_irradiance =
            global_horizontal_irradiance - direct_horizontal_irradiance;

        ClearSkyIrradiance {
            irradiance: Irradiance {
                diffuse_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                    diffuse_horizontal_irradiance,
                ),
                direct_normal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                    direct_normal_irradiance,
                ),
                global_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                    global_horizontal_irradiance,
                ),
            },
            latitude: lat,
            longitude: lon,
            utc: *utc,
            albedo: *albedo,
            solar_azimuth: azimuth,
            solar_zenith: zenith,
        }
    }

    // Get a clear sky model irradiance for a given location and time
    //
    // # Returns
    // * `Irradiance` - clear sky irradiance for given location and time
    pub fn get_clear_sky_irradiance(&self) -> Irradiance {
        self.irradiance
    }

    // Separate ghi into direct and diffuse components using engerer2 model
    //   Engerer - Minute resolution estimates of the diffuse fraction of global irradiance for southeastern Australia
    //   https://github.com/JamieMBright/Engerer2-separation-model
    //
    // # Arguments
    // * `ghi` - global horizontal irradiance from cloud sky model
    // * `averaging_period` - averaging period
    //
    // # Returns
    // * `Irradiance` - Irradiance struct with the cloud sky irradiance
    fn _separate_ghi(&self, ghi: &HeatFluxDensity, averaging_period: &Time) -> Irradiance {
        let time = self.utc.hour() as f64
            + self.utc.minute() as f64 / 60.0
            + self.utc.second() as f64 / 3600.0;
        let day_of_year = self.utc.ordinal() as f64;
        let dni_extra: HeatFluxDensity = get_extraterrestrial_radiation(&self.utc);
        let zenith: Angle = self.solar_zenith;

        let beta_equation_of_time = (360.0 / 365.242) * (day_of_year - 1.0);
        let equation_of_time = 0.258 * (std::f64::consts::PI / 180.0 * beta_equation_of_time).cos()
            - 7.416 * (std::f64::consts::PI / 180.0 * beta_equation_of_time).sin()
            - 3.648 * (std::f64::consts::PI / 180.0 * 2.0 * beta_equation_of_time).cos()
            - 9.228 * (std::f64::consts::PI / 180.0 * 2.0 * beta_equation_of_time).sin();
        let local_solar_noon = 12.0 - self.longitude / 15.0 + equation_of_time / 60.0;
        let hour_angle = (time - local_solar_noon) * 15.0;
        let hour_angle = if hour_angle >= 180.0 {
            hour_angle - 360.0
        } else if hour_angle <= -180.0 {
            hour_angle + 360.0
        } else {
            hour_angle
        };

        let cloud_clearness_index =
            ghi.get::<watt_per_square_meter>() / dni_extra.get::<watt_per_square_meter>();
        let clear_clearness_index = self
            .irradiance
            .direct_normal_irradiance
            .get::<watt_per_square_meter>()
            / dni_extra.get::<watt_per_square_meter>();
        let clearness_index_deviation = cloud_clearness_index - clear_clearness_index;

        let apparent_solar_time = (hour_angle / 15.0 + local_solar_noon).abs();

        let cloud_enhancement_estimate = ghi.get::<watt_per_square_meter>()
            - self
                .irradiance
                .global_horizontal_irradiance
                .get::<watt_per_square_meter>();
        let cloud_enhancement_estimate = if cloud_enhancement_estimate < 0.015 {
            0.0
        } else {
            cloud_enhancement_estimate
        };

        let engerer2_coefficients = match averaging_period.get::<minute>() as u32 {
            1 => (
                0.105620,
                -4.13320,
                8.25780,
                0.0100870,
                0.000888010,
                -4.93020,
                0.443780,
            ),
            5 => (
                0.0939360, -4.57710, 8.46410, 0.0100120, 0.00397500, -4.39210, 0.393310,
            ),
            10 => (
                0.0799650, -4.85390, 8.47640, 0.0188490, 0.00514970, -4.14570, 0.374660,
            ),
            15 => (
                0.0659720, -4.72110, 8.32940, 0.00954440, 0.00534930, -4.16900, 0.395260,
            ),
            30 => (
                0.0326750, -4.86810, 8.18670, 0.0158290, 0.00599220, -4.03040, 0.473710,
            ),
            60 => (
                -0.00975390,
                -5.31690,
                8.50840,
                0.0132410,
                0.00743560,
                -3.03290,
                0.564030,
            ),
            1440 => (
                0.327260, -9.43910, 17.1130, 0.137520, -0.0240990, 6.62570, 0.314190,
            ),
            _ => panic!("Averaging period must be 1, 5, 10, 15, 30, 60, or 1440 minutes"),
        };

        let engerer2 = (engerer2_coefficients.0
            + (1.0 - engerer2_coefficients.0)
                / (1.0
                    + (engerer2_coefficients.1
                        + engerer2_coefficients.2 * cloud_clearness_index
                        + engerer2_coefficients.3 * apparent_solar_time
                        + engerer2_coefficients.4 * zenith.get::<radian>()
                        + engerer2_coefficients.5 * clearness_index_deviation)
                        .exp())
            + engerer2_coefficients.6 * cloud_enhancement_estimate)
            .min(1.0)
            .max(0.0);

        let dif = ghi.get::<watt_per_square_meter>() * engerer2;
        let dni = (ghi.get::<watt_per_square_meter>() - dif) / zenith.cos().get::<ratio>();

        Irradiance {
            diffuse_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(dif),
            direct_normal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(dni),
            global_horizontal_irradiance: HeatFluxDensity::new::<watt_per_square_meter>(
                ghi.get::<watt_per_square_meter>(),
            ),
        }
    }

    // Calculate the cloud sky irradiance based on clear sky model.
    //   Skeie, Gustavsen - Predicting solar radiation using a parametric cloud model
    //   McGuffie, Henderson-Sellers - A Climate Modelling Primer, Third Edition
    //
    // # Arguments
    // * `low_clods` - Low cloud cover percentage
    // * `medium_clods` - Medium cloud cover percentage
    // * `high_clods` - High cloud cover percentage
    // * `total_cloud_cover` - Total cloud cover percentage
    // * `rain` - Rain true/false
    // * `averaging_period` - Averaging period for the ghi separation model
    //
    // # Returns
    // * `Irradiance` - Irradiance struct with the cloud sky irradiance
    pub fn get_cloud_sky_irradiance(
        &self,
        low_clouds: &Ratio,
        medium_clouds: &Ratio,
        high_clouds: &Ratio,
        total_cloud_coverage: &Ratio,
        rain: bool,
        averaging_period: &Time,
    ) -> Irradiance {
        let rain_transmittance = if rain { -0.25 } else { 0.0 };
        let low_clouds = (1.0 - low_clouds.get::<ratio>())
            + (LOW_CLOUDS_TRANSMITTANCE + rain_transmittance) * low_clouds.get::<ratio>();
        let medium_clouds = (1.0 - medium_clouds.get::<ratio>())
            + (MEDIUM_CLOUDS_TRANSMITTANCE + rain_transmittance) * medium_clouds.get::<ratio>();
        let high_clouds = (1.0 - high_clouds.get::<ratio>())
            + (HIGH_CLOUDS_TRANSMITTANCE + rain_transmittance) * high_clouds.get::<ratio>();

        let cloud_albedo = (LOW_CLOUDS_ALBEDO * low_clouds
            + MEDIUM_CLOUDS_ALBEDO * medium_clouds
            + HIGH_CLOUDS_ALBEDO * high_clouds)
            / (low_clouds + medium_clouds + high_clouds);

        // to make things simpler the aerosol scattering below clouds is ignored
        let atmospheric_albedo = (CLEAR_SKY_MOLECULAR_SCATTERING
            * (1.0 - total_cloud_coverage.get::<ratio>())) // molecular scattering affecting the clear sky part
            + cloud_albedo * total_cloud_coverage.get::<ratio>(); // cloud albedo affecting the cloudy part
        let ground_surface_albedo = self.albedo;

        // this is the global horizontal irradiance. It needs to be split into direct and diffuse
        let cloud_ghi: HeatFluxDensity = HeatFluxDensity::new::<watt_per_square_meter>(
            self.irradiance
                .global_horizontal_irradiance
                .get::<watt_per_square_meter>()
                * (low_clouds * medium_clouds * high_clouds)
                / (1.0 - atmospheric_albedo * ground_surface_albedo),
        );

        self._separate_ghi(&cloud_ghi, averaging_period)
    }
}

#[cfg(test)]
mod tests {
    use nalgebra::{assert_approx_eq_eps, ApproxEq, Vector3};
    use uom::si::angle::degree;
    use uom::si::f64::Angle;

    #[test]
    fn test_get_90_deg_north_wall_normal() {
        let azimuth = Angle::new::<degree>(0_f64);
        let wall_angle = Angle::new::<degree>(90_f64);
        let normal = super::get_vector_from_azimuth_zenith(&azimuth, &wall_angle);
        assert_approx_eq_eps!(Vector3::new(1.0, 0.0, 0.0), normal, 0.1);
    }

    #[test]
    fn test_get_illumination_coef_direct_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.0, 0.0, 1.0);
        let coef = super::get_illumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(1.0, coef, 0.1);
    }

    #[test]
    fn test_get_illumination_coef_45deg_sunlight() {
        let sun_vector = Vector3::new(0.0, 0.0, 1.0);
        let surface_normal = Vector3::new(0.7071067811865475, 0.0, 0.7071067811865476);
        let coef = super::get_illumination_coefficient(&sun_vector, &surface_normal);
        assert_approx_eq_eps!(0.7071, coef, 0.1);
    }

    #[test]
    fn test_get_vector_from_azimuth() {
        let azimuth = Angle::new::<degree>(90_f64);
        let zenith = Angle::new::<degree>(90_f64);
        let elevation = Angle::new::<degree>(0_f64);
        let vector1 = super::get_vector_from_azimuth_zenith(&azimuth, &zenith);
        let vector2 = super::get_vector_from_azimuth_elevation(&azimuth, &elevation);
        assert_approx_eq_eps!(vector1, vector2, 0.1);
    }

    #[test]
    fn test_projection() {
        let surface_azimuth: Angle = Angle::new::<degree>(180_f64);
        let surface_angle: Angle = Angle::new::<degree>(35_f64);
        let solar_azimuth: Angle = Angle::new::<degree>(180_f64);
        let solar_zenith: Angle = Angle::new::<degree>(0_f64);
        let dot_product = super::get_projection(
            &surface_azimuth,
            &surface_angle,
            &solar_azimuth,
            &solar_zenith,
        );
        assert_approx_eq_eps!(dot_product, 0.5735, 0.0001);
    }
}

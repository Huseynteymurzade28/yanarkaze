//! Drivers for common I2C room sensors: Bosch BME280 / BMP280, Sensirion SHT3x and
//! Aosong AHT20. Each is a few register reads plus the maker's conversion formula; the
//! formulas and checksums are plain functions with tests against the datasheet examples.
//!
//! Everything goes through the [`System`] I2C methods, so any board with I2C gets them.

use embassy_time::Timer;

use crate::platform::{HwError, System};

/// One measurement. Humidity and pressure are `None` when the sensor has no such element.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvReading {
    pub temp_c: f32,
    pub humidity: Option<f32>,
    pub pressure_hpa: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bme280,
    Bmp280,
    Sht3x,
    Aht20,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bme280 => "BME280",
            Kind::Bmp280 => "BMP280",
            Kind::Sht3x => "SHT3x",
            Kind::Aht20 => "AHT20",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    pub addr: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvError {
    Bus(HwError),
    /// The data arrived but its checksum is wrong: noise on the bus, or loose wires.
    Checksum,
    /// The sensor never finished its measurement.
    Busy,
}

impl From<HwError> for EnvError {
    fn from(e: HwError) -> Self {
        EnvError::Bus(e)
    }
}

impl core::fmt::Display for EnvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EnvError::Bus(e) => write!(f, "{e}"),
            EnvError::Checksum => f.write_str("checksum mismatch (noise or a loose wire)"),
            EnvError::Busy => f.write_str("the sensor did not finish measuring"),
        }
    }
}

/// Looks for the supported sensors at their usual addresses.
pub async fn detect<S: System>(sys: &mut S) -> heapless::Vec<Found, 4> {
    let mut found = heapless::Vec::new();
    for addr in [0x76, 0x77] {
        let mut id = [0u8];
        if sys.i2c_write_read(addr, &[bme280::REG_ID], &mut id).await.is_ok() {
            let kind = match id[0] {
                bme280::ID_BME280 => Some(Kind::Bme280),
                0x56..=0x58 => Some(Kind::Bmp280),
                _ => None, // e.g. a BMP180 (0x55), which works differently
            };
            if let Some(kind) = kind {
                let _ = found.push(Found { kind, addr });
            }
        }
    }
    // The SHT3x has no ID register, but its status word comes with a CRC, which another
    // chip at the same address (an INA219, say) would not get right.
    for addr in [0x44, 0x45] {
        if sht3x::status(sys, addr).await.is_ok() {
            let _ = found.push(Found { kind: Kind::Sht3x, addr });
        }
    }
    // The AHT20 has neither; it answers a read with its status byte at any time.
    let mut status = [0u8];
    if sys.i2c_read(0x38, &mut status).await.is_ok() {
        let _ = found.push(Found { kind: Kind::Aht20, addr: 0x38 });
    }
    found
}

/// Takes one measurement (about 10 ms for the BME280, 20 ms for the SHT3x, 80 ms for the AHT20).
pub async fn read<S: System>(sys: &mut S, sensor: Found) -> Result<EnvReading, EnvError> {
    match sensor.kind {
        Kind::Bme280 | Kind::Bmp280 => bme280::read(sys, sensor.addr, sensor.kind == Kind::Bme280).await,
        Kind::Sht3x => sht3x::read(sys, sensor.addr).await,
        Kind::Aht20 => aht20::read(sys, sensor.addr).await,
    }
}

/// CRC-8 with polynomial 0x31 and initial value 0xFF, used by both Sensirion and Aosong.
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0xFFu8;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x31 } else { crc << 1 };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Bosch BME280 / BMP280
// ---------------------------------------------------------------------------

pub mod bme280 {
    use super::*;

    pub const REG_ID: u8 = 0xD0;
    pub const ID_BME280: u8 = 0x60;
    const REG_CALIB_TP: u8 = 0x88; // 26 bytes: T1..T3, P1..P9, (0xA0), H1
    const REG_CALIB_H: u8 = 0xE1; // 7 bytes: H2..H6
    const REG_CTRL_HUM: u8 = 0xF2;
    const REG_STATUS: u8 = 0xF3;
    const REG_CTRL_MEAS: u8 = 0xF4;
    const REG_DATA: u8 = 0xF7; // press[3], temp[3], hum[2]

    /// The factory trimming values, burnt into each chip.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Calib {
        pub t1: u16,
        pub t2: i16,
        pub t3: i16,
        pub p1: u16,
        pub p2: i16,
        pub p3: i16,
        pub p4: i16,
        pub p5: i16,
        pub p6: i16,
        pub p7: i16,
        pub p8: i16,
        pub p9: i16,
        pub h1: u8,
        pub h2: i16,
        pub h3: u8,
        pub h4: i16,
        pub h5: i16,
        pub h6: i8,
    }

    impl Calib {
        /// From registers 0x88..=0xA1 and (BME280 only) 0xE1..=0xE7.
        pub fn parse(tp: &[u8; 26], h: &[u8; 7]) -> Self {
            let u = |i: usize| u16::from_le_bytes([tp[i], tp[i + 1]]);
            let s = |i: usize| i16::from_le_bytes([tp[i], tp[i + 1]]);
            Calib {
                t1: u(0),
                t2: s(2),
                t3: s(4),
                p1: u(6),
                p2: s(8),
                p3: s(10),
                p4: s(12),
                p5: s(14),
                p6: s(16),
                p7: s(18),
                p8: s(20),
                p9: s(22),
                h1: tp[25],
                h2: i16::from_le_bytes([h[0], h[1]]),
                h3: h[2],
                // H4 and H5 are 12-bit signed values sharing the nibbles of 0xE5.
                h4: ((h[3] as i8 as i16) << 4) | (h[4] & 0x0F) as i16,
                h5: ((h[5] as i8 as i16) << 4) | (h[4] >> 4) as i16,
                h6: h[6] as i8,
            }
        }
    }

    /// Raw ADC values from registers 0xF7.. (20-bit pressure and temperature, 16-bit humidity).
    pub fn parse_raw(data: &[u8; 8]) -> (i32, i32, i32) {
        let p = ((data[0] as i32) << 12) | ((data[1] as i32) << 4) | (data[2] as i32 >> 4);
        let t = ((data[3] as i32) << 12) | ((data[4] as i32) << 4) | (data[5] as i32 >> 4);
        let h = ((data[6] as i32) << 8) | data[7] as i32;
        (p, t, h)
    }

    /// Temperature in 0.01 degC, and `t_fine`, which the other two formulas need.
    /// Bosch BME280 datasheet, section 4.2.3 (32-bit integer version).
    pub fn compensate_t(c: &Calib, adc_t: i32) -> (i32, i32) {
        let (t1, t2, t3) = (c.t1 as i32, c.t2 as i32, c.t3 as i32);
        let var1 = (((adc_t >> 3) - (t1 << 1)) * t2) >> 11;
        let var2 = (((((adc_t >> 4) - t1) * ((adc_t >> 4) - t1)) >> 12) * t3) >> 14;
        let t_fine = var1 + var2;
        ((t_fine * 5 + 128) >> 8, t_fine)
    }

    /// Pressure in Pa as Q24.8 (divide by 256). Datasheet section 4.2.3 (64-bit version).
    pub fn compensate_p(c: &Calib, adc_p: i32, t_fine: i32) -> u32 {
        let mut var1 = t_fine as i64 - 128000;
        let mut var2 = var1 * var1 * c.p6 as i64;
        var2 += (var1 * c.p5 as i64) << 17;
        var2 += (c.p4 as i64) << 35;
        var1 = ((var1 * var1 * c.p3 as i64) >> 8) + ((var1 * c.p2 as i64) << 12);
        var1 = (((1i64 << 47) + var1) * c.p1 as i64) >> 33;
        if var1 == 0 {
            return 0; // avoid dividing by zero (only with an all-zero calibration)
        }
        let mut p = 1_048_576 - adc_p as i64;
        p = (((p << 31) - var2) * 3125) / var1;
        var1 = (c.p9 as i64 * (p >> 13) * (p >> 13)) >> 25;
        var2 = (c.p8 as i64 * p) >> 19;
        p = ((p + var1 + var2) >> 8) + ((c.p7 as i64) << 4);
        p as u32
    }

    /// Relative humidity as Q22.10 (divide by 1024). Datasheet section 4.2.3. The original
    /// is 32-bit; i64 gives the same results without any risk of overflow.
    pub fn compensate_h(c: &Calib, adc_h: i32, t_fine: i32) -> u32 {
        let (h1, h2, h3) = (c.h1 as i64, c.h2 as i64, c.h3 as i64);
        let (h4, h5, h6) = (c.h4 as i64, c.h5 as i64, c.h6 as i64);
        let x = t_fine as i64 - 76800;
        let a = ((((adc_h as i64) << 14) - (h4 << 20) - (h5 * x)) + 16384) >> 15;
        let b = ((((((x * h6) >> 10) * (((x * h3) >> 11) + 32768)) >> 10) + 2_097_152) * h2 + 8192) >> 14;
        let mut v = a * b;
        v -= ((((v >> 15) * (v >> 15)) >> 7) * h1) >> 4;
        (v.clamp(0, 419_430_400) >> 12) as u32
    }

    pub(super) async fn read<S: System>(sys: &mut S, addr: u8, humidity: bool) -> Result<EnvReading, EnvError> {
        let mut tp = [0u8; 26];
        sys.i2c_write_read(addr, &[REG_CALIB_TP], &mut tp).await?;
        let mut h = [0u8; 7];
        if humidity {
            sys.i2c_write_read(addr, &[REG_CALIB_H], &mut h).await?;
            // Humidity oversampling only takes effect after a write to ctrl_meas, below.
            sys.i2c_write(addr, &[REG_CTRL_HUM, 0x01]).await?;
        }
        let calib = Calib::parse(&tp, &h);
        // Temperature and pressure oversampling x1, forced mode: one measurement, then sleep.
        const OSRS_T_X1: u8 = 0b001 << 5;
        const OSRS_P_X1: u8 = 0b001 << 2;
        const MODE_FORCED: u8 = 0b01;
        sys.i2c_write(addr, &[REG_CTRL_MEAS, OSRS_T_X1 | OSRS_P_X1 | MODE_FORCED]).await?;
        // At x1 a measurement takes at most 9.3 ms; poll the "measuring" bit to be sure.
        Timer::after_millis(10).await;
        let mut done = false;
        for _ in 0..10 {
            let mut status = [0u8];
            sys.i2c_write_read(addr, &[REG_STATUS], &mut status).await?;
            if status[0] & 0x08 == 0 {
                done = true;
                break;
            }
            Timer::after_millis(2).await;
        }
        if !done {
            return Err(EnvError::Busy);
        }
        let mut data = [0u8; 8];
        let len = if humidity { 8 } else { 6 };
        sys.i2c_write_read(addr, &[REG_DATA], &mut data[..len]).await?;
        let (adc_p, adc_t, adc_h) = parse_raw(&data);
        let (t, t_fine) = compensate_t(&calib, adc_t);
        Ok(EnvReading {
            temp_c: t as f32 / 100.0,
            humidity: humidity.then(|| compensate_h(&calib, adc_h, t_fine) as f32 / 1024.0),
            pressure_hpa: Some(compensate_p(&calib, adc_p, t_fine) as f32 / 256.0 / 100.0),
        })
    }
}

// ---------------------------------------------------------------------------
// Sensirion SHT3x (SHT30, SHT31, SHT35)
// ---------------------------------------------------------------------------

pub mod sht3x {
    use super::*;

    /// Single shot, high repeatability, no clock stretching (datasheet table 9).
    const MEASURE: [u8; 2] = [0x24, 0x00];

    /// Checks both CRCs and converts (datasheet section 4.13).
    pub fn convert(data: &[u8; 6]) -> Result<EnvReading, EnvError> {
        if crc8(&data[0..2]) != data[2] || crc8(&data[3..5]) != data[5] {
            return Err(EnvError::Checksum);
        }
        let st = u16::from_be_bytes([data[0], data[1]]) as f32;
        let srh = u16::from_be_bytes([data[3], data[4]]) as f32;
        Ok(EnvReading {
            temp_c: -45.0 + 175.0 * st / 65535.0,
            humidity: Some(100.0 * srh / 65535.0),
            pressure_hpa: None,
        })
    }

    /// Reads the status register (datasheet table 17); an answer with a valid CRC means an
    /// SHT3x is there.
    pub(super) async fn status<S: System>(sys: &mut S, addr: u8) -> Result<u16, EnvError> {
        sys.i2c_write(addr, &[0xF3, 0x2D]).await?;
        let mut data = [0u8; 3];
        sys.i2c_read(addr, &mut data).await?;
        if crc8(&data[..2]) != data[2] {
            return Err(EnvError::Checksum);
        }
        Ok(u16::from_be_bytes([data[0], data[1]]))
    }

    pub(super) async fn read<S: System>(sys: &mut S, addr: u8) -> Result<EnvReading, EnvError> {
        sys.i2c_write(addr, &MEASURE).await?;
        Timer::after_millis(16).await; // 15.5 ms maximum at high repeatability
        let mut data = [0u8; 6];
        // Without clock stretching the sensor NACKs its address until it is done.
        for _ in 0..5 {
            match sys.i2c_read(addr, &mut data).await {
                Ok(()) => return convert(&data),
                Err(HwError::Nack) => Timer::after_millis(5).await,
                Err(e) => return Err(e.into()),
            }
        }
        Err(EnvError::Busy)
    }
}

// ---------------------------------------------------------------------------
// Aosong AHT20 (and the AHT21 / AHT25)
// ---------------------------------------------------------------------------

pub mod aht20 {
    use super::*;

    const INIT: [u8; 3] = [0xBE, 0x08, 0x00];
    const MEASURE: [u8; 3] = [0xAC, 0x33, 0x00];
    const BUSY: u8 = 0x80;
    const CALIBRATED: u8 = 0x08;

    /// Checks the CRC and unpacks the two 20-bit values (datasheet section 6).
    pub fn convert(data: &[u8; 7]) -> Result<EnvReading, EnvError> {
        if crc8(&data[..6]) != data[6] {
            return Err(EnvError::Checksum);
        }
        let srh = ((data[1] as u32) << 12) | ((data[2] as u32) << 4) | (data[3] as u32 >> 4);
        let st = (((data[3] & 0x0F) as u32) << 16) | ((data[4] as u32) << 8) | data[5] as u32;
        const FULL: f32 = (1u32 << 20) as f32;
        Ok(EnvReading {
            temp_c: st as f32 / FULL * 200.0 - 50.0,
            humidity: Some(srh as f32 / FULL * 100.0),
            pressure_hpa: None,
        })
    }

    pub(super) async fn read<S: System>(sys: &mut S, addr: u8) -> Result<EnvReading, EnvError> {
        let mut status = [0u8];
        sys.i2c_read(addr, &mut status).await?;
        if status[0] & CALIBRATED == 0 {
            sys.i2c_write(addr, &INIT).await?;
            Timer::after_millis(10).await;
        }
        sys.i2c_write(addr, &MEASURE).await?;
        Timer::after_millis(80).await; // the datasheet's measuring time
        let mut data = [0u8; 7];
        for _ in 0..5 {
            sys.i2c_read(addr, &mut data).await?;
            if data[0] & BUSY == 0 {
                return convert(&data);
            }
            Timer::after_millis(10).await;
        }
        Err(EnvError::Busy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_sensirion_example() {
        // SHT3x datasheet, table 19: CRC(0xBEEF) = 0x92.
        assert_eq!(crc8(&[0xBE, 0xEF]), 0x92);
        assert_eq!(crc8(&[]), 0xFF);
    }

    /// The worked example from the BMP280 datasheet (section 3.12), whose temperature and
    /// pressure formulas are the same as the BME280's.
    fn bmp280_example() -> bme280::Calib {
        bme280::Calib {
            t1: 27504,
            t2: 26435,
            t3: -1000,
            p1: 36477,
            p2: -10685,
            p3: 3024,
            p4: 2855,
            p5: 140,
            p6: -7,
            p7: 15500,
            p8: -14600,
            p9: 6000,
            ..Default::default()
        }
    }

    #[test]
    fn bme280_temperature_and_pressure_match_the_datasheet() {
        let c = bmp280_example();
        let (t, t_fine) = bme280::compensate_t(&c, 519888);
        assert_eq!(t_fine, 128422);
        assert_eq!(t, 2508); // 25.08 degC
        let p = bme280::compensate_p(&c, 415148, t_fine) as f32 / 256.0;
        assert!((p - 100653.27).abs() < 1.0, "{p} Pa");
    }

    #[test]
    fn bme280_calibration_layout() {
        let mut tp = [0u8; 26];
        tp[0..2].copy_from_slice(&27504u16.to_le_bytes());
        tp[2..4].copy_from_slice(&26435i16.to_le_bytes());
        tp[4..6].copy_from_slice(&(-1000i16).to_le_bytes());
        tp[22..24].copy_from_slice(&6000i16.to_le_bytes());
        tp[25] = 75; // H1 at 0xA1
        // H2 = 370, H3 = 0, H4 = 313 (0x139), H5 = 50 (0x032), H6 = 30: a typical chip.
        let h = [0x72, 0x01, 0x00, 0x13, 0x29, 0x03, 0x1E];
        let c = bme280::Calib::parse(&tp, &h);
        assert_eq!((c.t1, c.t2, c.t3, c.p9), (27504, 26435, -1000, 6000));
        assert_eq!((c.h1, c.h2, c.h3, c.h4, c.h5, c.h6), (75, 370, 0, 313, 50, 30));
        // Negative 12-bit values keep their sign.
        let c = bme280::Calib::parse(&tp, &[0, 0, 0, 0xFF, 0xF2, 0xFF, 0]);
        assert_eq!((c.h4, c.h5), (-14, -1));
    }

    #[test]
    fn bme280_raw_layout() {
        let (p, t, h) = bme280::parse_raw(&[0x65, 0x5A, 0xC0, 0x7E, 0xED, 0x00, 0x6B, 0x6C]);
        assert_eq!(p, 0x655AC);
        assert_eq!(t, 0x7EED0);
        assert_eq!(h, 0x6B6C);
    }

    #[test]
    fn bme280_humidity_is_sane() {
        let mut c = bmp280_example();
        (c.h1, c.h2, c.h3, c.h4, c.h5, c.h6) = (75, 370, 0, 313, 50, 30);
        let (_, t_fine) = bme280::compensate_t(&c, 519888);
        let rh = |adc| bme280::compensate_h(&c, adc, t_fine) as f32 / 1024.0;
        // More raw counts mean more humidity, and the result stays within 0..=100 %.
        let (low, mid, high) = (rh(20000), rh(27500), rh(35000));
        assert!(low < mid && mid < high, "{low} {mid} {high}");
        assert!((0.0..=100.0).contains(&low) && (0.0..=100.0).contains(&high));
        assert_eq!(rh(0), 0.0);
        assert_eq!(rh(65535), 100.0);
    }

    #[test]
    fn sht3x_conversion() {
        // 0x6666 is 40 % of full scale: -45 + 175 * 0.4 = 25 degC; 0x8000 is about 50 %RH.
        let data = [0x66, 0x66, crc8(&[0x66, 0x66]), 0x80, 0x00, crc8(&[0x80, 0x00])];
        let r = sht3x::convert(&data).unwrap();
        assert!((r.temp_c - 25.0).abs() < 0.01);
        assert!((r.humidity.unwrap() - 50.0).abs() < 0.01);
        let mut bad = data;
        bad[1] ^= 1;
        assert_eq!(sht3x::convert(&bad), Err(EnvError::Checksum));
    }

    #[test]
    fn aht20_conversion() {
        // Humidity 0x80000 (50 %), temperature 0x60000 (0.375 * 200 - 50 = 25 degC).
        let mut data = [0x1C, 0x80, 0x00, 0x06, 0x00, 0x00, 0];
        data[6] = crc8(&data[..6]);
        let r = aht20::convert(&data).unwrap();
        assert!((r.humidity.unwrap() - 50.0).abs() < 0.01);
        assert!((r.temp_c - 25.0).abs() < 0.01);
        data[6] ^= 0xFF;
        assert_eq!(aht20::convert(&data), Err(EnvError::Checksum));
    }
}

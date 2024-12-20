// Copyright (c) 2023 Angus Gratton
// SPDX-License-Identifier: GPL-2.0-or-later
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use itertools::Itertools;
use serde::Deserialize;

use crate::video::SourceFrame;
use crate::Nanos;

// Wrapper enum for all inputs to the route log
#[derive(Eq)]
pub enum LogInput {
    CAN(CANMessage),
    Frame(SourceFrame),
    Alert(Alert),
}

impl LogInput {
    // Return timestamp in nanoseconds
    pub fn timestamp(&self) -> Nanos {
        match self {
            LogInput::CAN(m) => m.timestamp,
            LogInput::Frame(s) => s.ts_ns,
            LogInput::Alert(s) => s.timestamp,
        }
    }
}

impl From<CANMessage> for LogInput {
    fn from(value: CANMessage) -> Self {
        LogInput::CAN(value)
    }
}

impl From<SourceFrame> for LogInput {
    fn from(value: SourceFrame) -> Self {
        LogInput::Frame(value)
    }
}

impl Ord for LogInput {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp().cmp(&other.timestamp())
    }
}

impl PartialOrd for LogInput {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for LogInput {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp() == other.timestamp()
    }
}

// Parser for CAN messages from CSV log
#[derive(Eq, PartialEq, Debug)]
pub struct CANMessage {
    pub timestamp: Nanos,
    pub can_id: u32,
    pub is_extended_id: bool,
    pub bus_no: u8,
    pub data: Vec<u8>,
}

impl Ord for CANMessage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp().cmp(&other.timestamp())
    }
}

impl PartialOrd for CANMessage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl CANMessage {
    pub fn parse_from(record: &csv::StringRecord, ts_offs: Nanos) -> Result<Self> {
        // in this format, each record has a variable number of fields
        // and we want to concatenate the variable data fields
        let mut fields = record.iter();

        let ts_us: i64 = fields.next().ok_or(anyhow!("Missing ts field"))?.parse()?;
        let can_id =
            u32::from_str_radix(fields.next().ok_or(anyhow!("Missing can id field"))?, 16)?;
        let is_extended_id = fields
            .next()
            .ok_or(anyhow!("Missing is_extended_id field"))?
            == "true";

        // SavvyCAN CSV files have a field here for Rx/Tx, skip it if present
        let maybe_tx_rx = fields.next();
        let next = if maybe_tx_rx != Some("Tx") && maybe_tx_rx != Some("Rx") {
            maybe_tx_rx
        } else {
            fields.next()
        };

        let bus_no = next
            .ok_or(anyhow!("Missing bus field"))?
            .parse()
            .context("Invalid bus field")?;
        fields.next(); // dlen field, can skip this one

        // collect the remaining variable number of data fields d1..d8
        let data = fields
            .take(8)
            .map(|d| u8::from_str_radix(d, 16))
            .try_collect()
            .context("Error parsing CSV data field")?;

        Ok(CANMessage {
            timestamp: (ts_us * 1000) as Nanos - ts_offs,
            can_id,
            is_extended_id,
            bus_no,
            data,
        })
    }

    pub fn timestamp(&self) -> Nanos {
        self.timestamp
    }
}

pub fn read_can_messages(
    csv_log_path: &Path,
    can_ts_offs: Option<Nanos>,
) -> Result<Vec<CANMessage>> {
    eprintln!("Opening CAN log {:?}...", csv_log_path);

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_path(csv_log_path)
        .with_context(|| format!("Failed to read CSV file {:?}", csv_log_path))?;

    let mut records = rdr.records().peekable();

    let can_ts_offs = can_ts_offs.unwrap_or_else(|| match records.peek() {
        // If no timestamp offset was specified, offset so the first message
        // has timestamp 0
        Some(Ok(record)) => match CANMessage::parse_from(record, 0) {
            Ok(message) => message.timestamp(),
            _ => 0,
        },
        _ => 0,
    });

    eprintln!("can_ts_offs {}", can_ts_offs);

    let mut result = records
        .enumerate()
        .map(|(row, rec)| match rec {
            Ok(r) => CANMessage::parse_from(&r, can_ts_offs).with_context(|| {
                format!(
                    "Invalid CAN data found in CSV {:?} row {}",
                    csv_log_path,
                    row + 1
                )
            }),
            Err(e) => Err(anyhow!(
                "Invalid CSV record in file {:?}: {}",
                csv_log_path,
                e
            )),
        })
        // TODO: For now dropping any CAN timestamp that comes before the video
        // started. Could conceivably adjust the start earlier instead and have empty video
        .filter(|r| match r {
            Ok(m) => m.timestamp >= 0,
            _ => true,
        })
        .collect::<Result<Vec<CANMessage>>>()?;
    // When the log contains >1 bus of data, the messages can be slightly out
    // of order
    result.sort();
    Ok(result)
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AlertStatus {
    Normal,
    UserPrompt,
    Critical,
}

#[derive(Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Alert {
    pub timestamp: Nanos,
    pub status: AlertStatus,
    pub message: Option<String>,
}

// Scan the CAN messages for gaps that may indicate faults in the CAN logging
pub fn find_missing_can_messages(messages: &[CANMessage]) -> Vec<Alert> {
    let mut result = vec![];
    let mut last_timestamp = messages.first().map(|m| m.timestamp()).unwrap_or(0);
    const MISSING_THRESHOLD: Nanos = 500_000_000;

    for m in messages {
        if m.timestamp() - last_timestamp > MISSING_THRESHOLD {
            let msg = format!(
                "Possible lost CAN messages.\nGap of {:.3}s with no message",
                (m.timestamp() - last_timestamp) as f64 / 1_000_000_000.0
            );
            result.push(Alert {
                status: AlertStatus::Critical,
                message: Some(msg),
                timestamp: last_timestamp,
            });
            result.push(Alert {
                status: AlertStatus::Normal,
                message: None,
                timestamp: m.timestamp(),
            });
        }
        last_timestamp = m.timestamp();
    }
    result
}

/* Takes a list of individual alerts and expands them to cover the whole video
 * time span, with one alert each 100ms. Each alert is repeated until the next
 * alert starts (recall some alerts have message None).
 *
 * This is necessary so they display in Cabana during playback.
 */
pub fn expand_alerts(alerts: Vec<Alert>) -> Vec<LogInput> {
    let first_ts = match alerts.first() {
        Some(first) => first.timestamp,
        _ => 0,
    };
    let last_ts = match alerts.last() {
        Some(last) => last.timestamp,
        _ => first_ts,
    };

    let mut result = vec![];

    let mut ts = first_ts;

    let mut peekable = alerts.into_iter().peekable();

    while let Some(alert) = peekable.next() {
        let next_at = peekable.peek().map(|a| a.timestamp).unwrap_or(last_ts);
        while ts < next_at {
            let mut new_alert = alert.clone();
            new_alert.timestamp = ts;
            result.push(LogInput::Alert(new_alert));
            ts += 100_000_000; // 100ms
        }
    }

    result
}

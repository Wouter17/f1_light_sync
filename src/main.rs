use std::env;
use std::io;
use std::time::Duration;
use std::time::Instant;

use f1_game_library_models_25::telemetry_data::EventType;
use f1_game_library_models_25::telemetry_data::F1Data;
use f1_game_library_models_25::telemetry_data::VehicleFiaFlags;
use tokio::net::UdpSocket;

const PENALTY_SHOW_TIME: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let Some(destination) = args.get(1) else {
        println!("Expected a destination");
        return Ok(());
    };
    let source_port = args.get(2).map(String::as_ref).unwrap_or("20888");

    let input_socket = UdpSocket::bind(format!("127.0.0.1:{}", source_port)).await?;
    let output_socket = UdpSocket::bind("0.0.0.0:0").await?;
    output_socket.connect(destination).await?;

    let mut buf = [0; 2048];

    let mut manager = FlagManager::new(output_socket);
    println!(
        "Listening to 127.0.0.1:{} and outputting on {}",
        source_port, destination
    );
    loop {
        let (len, _) = input_socket.recv_from(&mut buf).await?;
        let Ok(packet) = f1_game_library_models_25::deserialise_udp_packet_from_bytes(&buf[..len])
        else {
            println!("Failed to parse packet");
            continue;
        };

        match packet {
            F1Data::ParticipantData(data) => {
                manager.driver_numbers = data.participants.map(|v| v.race_number)
            }
            F1Data::EventData(data) => match data.r#type {
                EventType::SafetyCar(safetycar) => {
                    match (safetycar.safety_car_type, safetycar.event_type) {
                        (0, _) | (_, 2) | (_, 3) => manager.reset_global_flag().await,
                        (1, x) | (3, x) if x == 0 || x == 1 => {
                            manager.set_global_flag(GlobalFlag::Sc).await
                        }
                        (2, 0) | (2, 1) => manager.set_global_flag(GlobalFlag::Vsc).await,
                        _ => unreachable!("all numbers should be in the range ([0,3], [0,3])"),
                    }
                }
                EventType::Penalty(penalty) => manager.set_penalty(penalty.vehicle_index).await,
                EventType::ChequeredFlag(_) => manager.finish().await,
                EventType::RedFlag(_) => manager.set_global_flag(GlobalFlag::Red).await,
                EventType::SessionStart(_) | EventType::SessionEnd(_) => manager.reset(),
                _ => (),
            },
            F1Data::ClassificationData(_) => manager.reset(),
            F1Data::CarStatusData(data) => {
                let driver_index = data.header.player_car_index;
                match data
                    .car_status_data
                    .get(driver_index)
                    .expect("driver index should be within maximum cars in session")
                    .vehicle_fia_flags
                {
                    VehicleFiaFlags::InvalidUnknown => println!("Unknown local flag received"),
                    VehicleFiaFlags::None => manager.reset_local_flag().await,
                    VehicleFiaFlags::Green => manager.set_local_flag(LocalFlag::Green).await,
                    VehicleFiaFlags::Blue => manager.set_local_flag(LocalFlag::Blue).await,
                    VehicleFiaFlags::Yellow => manager.set_local_flag(LocalFlag::Yellow).await,
                    VehicleFiaFlags::Red => manager.set_global_flag(GlobalFlag::Red).await,
                }
            }
            _ => (),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GlobalFlag {
    Vsc,
    Sc,
    Red,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LocalFlag {
    Green,
    Yellow,
    Blue,
}

#[derive(Debug, Clone, Copy)]
enum Flag {
    Global(GlobalFlag),
    Local(LocalFlag),
    Penalty(usize),
    Finish,
}

impl Flag {
    fn to_enum_str(self) -> String {
        match self {
            Flag::Global(global_flag) => String::from(match global_flag {
                GlobalFlag::Vsc => "5",
                GlobalFlag::Sc => "4",
                GlobalFlag::Red => "12",
            }),
            Flag::Local(local_flag) => String::from(match local_flag {
                LocalFlag::Green => "1",
                LocalFlag::Yellow => "2",
                LocalFlag::Blue => "8",
            }),
            Flag::Penalty(index) => format!("11,{index}"),
            Flag::Finish => String::from("16"),
        }
    }
}

impl From<LocalFlag> for Flag {
    fn from(value: LocalFlag) -> Self {
        Self::Local(value)
    }
}

impl From<GlobalFlag> for Flag {
    fn from(value: GlobalFlag) -> Self {
        Self::Global(value)
    }
}

#[derive(Debug)]
struct FlagManager {
    global_flag: Option<GlobalFlag>,
    local_flag: Option<LocalFlag>,
    race_finished: bool,
    showing_penalty_since: Option<Instant>,
    driver_numbers: [u8; f1_game_library_models_25::constants::MAX_CARS_IN_SESSION],
    output_socket: UdpSocket,
}

fn show_based_on_local(
    flag: Option<LocalFlag>,
    penalty: bool,
    finished: bool,
) -> Option<Option<LocalFlag>> {
    match flag {
        None if !penalty && !finished => Some(None),
        Some(LocalFlag::Green) if !finished => Some(flag),
        Some(LocalFlag::Yellow) | Some(LocalFlag::Blue) => Some(flag),
        _ => None,
    }
}

impl FlagManager {
    fn new(output_socket: UdpSocket) -> Self {
        Self {
            global_flag: Default::default(),
            local_flag: Default::default(),
            race_finished: Default::default(),
            showing_penalty_since: Default::default(),
            driver_numbers: Default::default(),
            output_socket,
        }
    }

    fn reset(&mut self) {
        self.global_flag = None;
        self.local_flag = None;
        self.race_finished = false;
        self.showing_penalty_since = None;
        self.driver_numbers = Default::default();
    }

    async fn show(&mut self, flag: Option<Flag>) {
        let string_input = flag.map(Flag::to_enum_str).unwrap_or_default();
        if self
            .output_socket
            .send(string_input.as_bytes())
            .await
            .is_err()
        {
            println!("Failed to send show command");
        };
    }

    async fn finish(&mut self) {
        self.race_finished = true;
        if self.global_flag.is_none() {
            self.show(Some(Flag::Finish)).await;
        }
    }

    async fn set_penalty(&mut self, index: usize) {
        self.showing_penalty_since = Some(Instant::now());
        if self.global_flag.is_none() {
            self.show(Some(Flag::Penalty(index))).await;
        }
    }

    fn check_penalty(&mut self) {
        if let Some(time) = self.showing_penalty_since
            && time.duration_since(Instant::now()) > PENALTY_SHOW_TIME
        {
            self.showing_penalty_since = None;
        }
    }

    async fn set_global_flag_value(&mut self, flag: Option<GlobalFlag>) {
        self.check_penalty();
        if self.global_flag == flag {
            return;
        }

        self.global_flag = flag;

        if flag.is_some() {
            return;
        }

        if let Some(local_flag) = show_based_on_local(
            self.local_flag,
            self.showing_penalty_since.is_some(),
            self.race_finished,
        ) {
            self.show(local_flag.map(Flag::from)).await;
        }
    }

    async fn set_local_flag_value(&mut self, flag: Option<LocalFlag>) {
        self.check_penalty();
        if self.local_flag == flag {
            return;
        }

        self.local_flag = flag;

        if self.global_flag.is_some() {
            return;
        }

        if let Some(local_flag) = show_based_on_local(
            flag,
            self.showing_penalty_since.is_some(),
            self.race_finished,
        ) {
            self.show(local_flag.map(Flag::from)).await;
        }
    }

    async fn set_global_flag(&mut self, flag: GlobalFlag) {
        self.set_global_flag_value(Some(flag)).await;
    }

    async fn reset_global_flag(&mut self) {
        self.set_global_flag_value(None).await;
    }

    async fn set_local_flag(&mut self, flag: LocalFlag) {
        self.set_local_flag_value(Some(flag)).await;
    }

    async fn reset_local_flag(&mut self) {
        self.set_local_flag_value(None).await;
    }
}

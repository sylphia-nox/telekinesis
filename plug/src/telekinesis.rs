use anyhow::Error;
use buttplug::{
    client::{ButtplugClient, ButtplugClientDevice, ButtplugClientEvent},
    core::{
        connector::{
            ButtplugConnector,
            ButtplugInProcessClientConnectorBuilder, new_json_ws_client_connector,
        },
        message::{
            ActuatorType, ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage,
        },
    },
    server::{
        device::hardware::communication::btleplug::BtlePlugCommunicationManagerBuilder,
        ButtplugServerBuilder,
    },
};
use futures::{Future, StreamExt};

use std::{
    fmt::{self},
    sync::{Arc, Mutex},
};
use tokio::{runtime::Runtime, sync::mpsc::channel, sync::mpsc::unbounded_channel};
use tracing::{debug, error, info, warn};

use itertools::Itertools;

use crate::{
    commands::{create_cmd_thread, TkAction, TkParams, TkDeviceSelector},
    inputs::sanitize_input_string,
    settings::{TkSettings, TkConnectionType},
    Speed, Tk, TkDuration, TkEvent, TkPattern, Telekinesis, TkConnectionStatus,
};

pub static ERROR_HANDLE: i32 = -1;

pub fn in_process_connector() -> impl ButtplugConnector<ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage> {
    ButtplugInProcessClientConnectorBuilder::default()
        .server(
            ButtplugServerBuilder::default()
                .comm_manager(BtlePlugCommunicationManagerBuilder::default())
                .finish()
                .expect("Could not create in-process-server."),
        )
        .finish()
}

impl Telekinesis {
    pub fn connect_with<T, Fn, Fut>(
        connector_factory: Fn,
        provided_settings: Option<TkSettings>,
    ) -> Result<Telekinesis, anyhow::Error>
    where
        Fn: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
        T: ButtplugConnector<ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage>
            + 'static,
    {
        let (event_sender, event_receiver) = unbounded_channel();
        let (command_sender, command_receiver) = channel(256); // we handle them immediately

        let devices = Arc::new(Mutex::new(vec![]));
        let devices_clone = devices.clone();

        let settings = provided_settings.or(Some(TkSettings::default())).unwrap();
        let pattern_path = settings.pattern_path.clone();

        let runtime = Runtime::new()?;
        runtime.spawn(async move {
            info!("Main thread started");
            let buttplug = with_connector(connector_factory().await).await;
            let mut events = buttplug.event_stream();
            create_cmd_thread(buttplug, event_sender.clone(), command_receiver, pattern_path);
            while let Some(event) = events.next().await {
                match event.clone() {
                    ButtplugClientEvent::DeviceAdded(device) => {
                        let mut device_list = devices_clone.lock().unwrap();
                        if !device_list
                            .iter()
                            .any(|d: &Arc<ButtplugClientDevice>| d.index() == device.index())
                        {
                            device_list.push(device);
                        }
                    }
                    ButtplugClientEvent::Error(err) => {
                        error!("Server error {:?}", err);
                    },
                    _ => {}
                };
                event_sender
                    .send(TkEvent::from_event(event))
                    .unwrap_or_else(|_| warn!("Dropped event cause queue is full."));
            }
        });

        Ok(Telekinesis {
            command_sender: command_sender,
            event_receiver: event_receiver,
            devices: devices,
            thread: runtime,
            settings: settings,
            connection_status: TkConnectionStatus::NotConnected,
            last_handle: 0
        })
    }

    fn get_next_handle(&mut self) -> i32 {
        self.last_handle += 1;
        self.last_handle
    }
}

impl fmt::Debug for Telekinesis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telekinesis").finish()
    }
}

impl Tk for Telekinesis {
    fn connect(settings: TkSettings) -> Result<Telekinesis, Error> {
        let settings_clone = settings.clone();
        match settings.connection {
            TkConnectionType::WebSocket(endpoint) => {
                let uri = format!("ws://{}", endpoint);
                info!("Connecting Websocket: {}", uri);
                Telekinesis::connect_with(|| async move { new_json_ws_client_connector(&uri) }, Some(settings_clone))
            },
            _ => {
                info!("Connecting In-Process");
                Telekinesis::connect_with(|| async move { in_process_connector() }, Some(settings))
            }
        }
    }

    fn scan_for_devices(&self) -> bool {
        info!("Sending Command: Scan for devices");
        if let Err(_) = self.command_sender.try_send(TkAction::Scan) {
            error!("Failed to start scan");
            return false;
        }
        true
    }

    fn stop_scan(&self) -> bool {
        info!("Sending Command: Stop scan");
        if let Err(_) = self.command_sender.try_send(TkAction::StopScan) {
            error!("Failed to stop scan");
            return false;
        }
        true
    }

    fn get_devices(&self) -> Vec<Arc<ButtplugClientDevice>> {
        self.devices
            .as_ref()
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.clone())
            .collect()
    }

    fn get_device_names(&self) -> Vec<String> {
        debug!("Getting devices names");
        self.get_devices()
            .iter()
            .map(|d| d.name().clone())
            .chain(self.settings.devices.iter().map(|d| d.name.clone()))
            .into_iter()
            .unique()
            .collect()
    }

    fn get_device_capabilities(&self, name: &str) -> Vec<String> {
        debug!("Getting '{}' capabilities", name);
        // maybe just return all actuator + types + linear + rotate
        if self
            .get_devices()
            .iter()
            .filter(|d| d.name() == name)
            .any(|device| {
                if let Some(scalar) = device.message_attributes().scalar_cmd() {
                    if scalar
                        .iter()
                        .any(|a| *a.actuator_type() == ActuatorType::Vibrate)
                    {
                        return true;
                    }
                }
                false
            })
        {
            return vec![ActuatorType::Vibrate.to_string()];
        }
        vec![]
    }

    fn vibrate(&mut self, speed: Speed, duration: TkDuration, events: Vec<String>) -> i32 {
        self.vibrate_pattern(TkPattern::Linear(duration, speed), events)
    }

    fn vibrate_pattern(&mut self, pattern: TkPattern, events: Vec<String>) -> i32 {
        info!("Received: Vibrate/Vibrate Pattern");
        let handle = self.get_next_handle();
        let selected =
            TkDeviceSelector::from_events(sanitize_input_string(events), &self.settings.devices);
        if let Err(_) = self.command_sender.try_send(TkAction::Control(handle, TkParams {
            selector: selected,
            pattern: pattern,
        })) {
            error!("Failed to send vibrate");
            return ERROR_HANDLE;
        }
        handle
    }

    fn vibrate_all(&mut self, speed: Speed, duration: TkDuration) -> i32 {
        info!("Received: Vibrate All");

        let handle = self.get_next_handle();
        if let Err(_) = self.command_sender.try_send(TkAction::Control(handle, TkParams {
            selector: TkDeviceSelector::All,
            pattern: TkPattern::Linear(duration, speed),
        })) {
            error!("Failed to queue vibrate");
            return ERROR_HANDLE;
        }
        handle
    }

    fn stop(&self, handle: i32) -> bool {
        info!("Received: Stop");
        if let Err(_) = self.command_sender.try_send(TkAction::Stop(handle)) {
            error!("Failed to queue stop");
            return false;
        }
        true
    }

    fn stop_all(&self) -> bool {
        info!("Received: Stop All");
        if let Err(_) = self.command_sender.try_send(TkAction::StopAll) {
            error!("Failed to queue stop_all");
            return false;
        }
        true
    }

    fn disconnect(&mut self) {
        info!("Sending Command: Disconnecting client");
        if let Err(_) = self.command_sender.try_send(TkAction::Disconect) {
            error!("Failed to send disconnect");
        }
    }

    fn get_next_event(&mut self) -> Option<TkEvent> {
        if let Ok(msg) = self.event_receiver.try_recv() {
            debug!("Got event {}", msg.to_string());
            match &msg {
                TkEvent::ScanFailed(err) => {
                    self.connection_status = TkConnectionStatus::Failed(err.to_string());
                },
                TkEvent::ScanStarted => {
                    self.connection_status = TkConnectionStatus::Connected;
                },
                _ => {}
            }
            return Some(msg);
        }
        None
    }

    fn process_next_events(&mut self) -> Vec<TkEvent> {
        debug!("Polling all events");
        let mut events = vec![];
        while let Some(event) = self.get_next_event() {
            events.push(event);
            if events.len() >= 128 {
                break;
            }
        }
        events
    }

    fn settings_set_enabled(&mut self, device_name: &str, enabled: bool) {
        info!("Setting '{}'.enabled={}", device_name, enabled);

        let mut settings = self.settings.clone();
        settings.set_enabled(device_name, enabled);
        self.settings = settings;
    }

    fn settings_set_events(&mut self, device_name: &str, events: Vec<String>) {
        info!("Setting '{}'.events={:?}", device_name, events);

        let settings = self.settings.clone();
        self.settings = settings.set_events(device_name, events);
    }

    fn settings_get_events(&self, device_name: &str) -> Vec<String> {
        self.settings.get_events(device_name)
    }

    fn get_device_connected(&self, device_name: &str) -> bool {
        debug!("Getting setting '{}' connected", device_name);
        self.get_devices().iter().any(|d| d.name() == device_name)
    }

    fn settings_get_enabled(&self, device_name: &str) -> bool {
        let enabled = self.settings.is_enabled(device_name);
        debug!("Getting setting '{}'.enabled={}", device_name, enabled);
        enabled
    }

}

async fn with_connector<T>(connector: T) -> ButtplugClient
where
    T: ButtplugConnector<ButtplugCurrentSpecClientMessage, ButtplugCurrentSpecServerMessage>
        + 'static,
{
    let buttplug = ButtplugClient::new("Telekinesis");
    let bp = buttplug.connect(connector).await;
    match bp {
        Ok(_) => {
            info!("Connected client.")
        }
        Err(err) => {
            error!("Could not connect client. Error: {}.", err);
        }
    }
    buttplug
}

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use std::{thread, time::Duration, vec};

    use crate::util::{enable_log};
    use crate::{
        fakes::{linear, scalar, FakeConnectorCallRegistry, FakeDeviceConnector},
        util::assert_timeout,
    };
    use buttplug::core::message::{ActuatorType, DeviceAdded};
    use lazy_static::__Deref;

    use super::*;

    impl Telekinesis {
        /// should only be used by tests or fake backends
        pub fn await_connect(&self, devices: usize) {
            assert_timeout!(
                self.devices.deref().lock().unwrap().deref().len() == devices,
                "Awaiting connect"
            );
        }
    }

    #[test]
    fn get_devices_contains_connected_devices() {
        // arrange
        let (tk, _) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Inflate),
        ]);

        // assert
        assert_timeout!(
            tk.devices.deref().lock().unwrap().deref().len() == 2,
            "Enough devices connected"
        );
        assert!(
            tk.get_device_names().contains(&String::from("vib1")),
            "Contains name vib1"
        );
        assert!(
            tk.get_device_names().contains(&String::from("vib2")),
            "Contains name vib2"
        );
    }

    #[test]
    fn get_devices_contains_devices_from_settings() {
        let (mut tk, _) = wait_for_connection(vec![]);
        tk.settings_set_enabled("foreign", true);
        assert!(
            tk.get_device_names().contains(&String::from("foreign")),
            "Contains additional device from settings"
        );
    }

    #[test]
    fn vibrate_all_demo_only_vibrates_vibrators() {
        // arrange
        let (connector, call_registry) = FakeDeviceConnector::device_demo();
        let count = connector.devices.len();

        // act
        let mut tk = Telekinesis::connect_with(|| async move { connector }, None).unwrap();
        tk.await_connect(count);
        tk.vibrate_all(Speed::new(100), TkDuration::from_millis(1));

        // assert
        call_registry.assert_vibrated(1); // scalar
        call_registry.assert_not_vibrated(4); // linear
        call_registry.assert_not_vibrated(7); // rotator
    }

    #[test]
    fn vibrate_all_only_vibrates_vibrators() {
        // arrange
        let (mut tk, call_registry) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Inflate),
        ]);

        tk.vibrate_all(Speed::new(100), TkDuration::from_millis(1));

        // assert
        call_registry.assert_vibrated(1);
        call_registry.assert_not_vibrated(2);
    }

    #[test]
    fn vibrate_non_existing_device() {
        // arrange
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        tk.vibrate(
            Speed::max(),
            TkDuration::from_millis(1),
            vec![String::from("does not exist")],
        );
        thread::sleep(Duration::from_millis(50));

        // assert
        call_registry.assert_not_vibrated(1);
    }

    #[test]
    fn vibrate_two_devices_simultaneously_both_are_started_and_stopped() {
        let (mut tk, call_registry) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Vibrate),
        ]);
        tk.settings_set_events("vib1", vec![String::from("device 1")]);
        tk.settings_set_events("vib2", vec![String::from("device 2")]);

        // act
        tk.vibrate(
            Speed::new(99),
            TkDuration::from_millis(3000),
            vec![String::from("device 1")],
        );
        tk.vibrate(
            Speed::new(88),
            TkDuration::from_millis(3000),
            vec![String::from("device 2")],
        );
        thread::sleep(Duration::from_secs(5));

        // assert
        call_registry.assert_vibrated(1);
        call_registry.assert_vibrated(2);
    }

    #[test]
    fn linear_correct_priority_2() {
        // call1  |111111111111111111111-->|
        // call2         |2222->|
        // result |111111122222211111111-->|

        // arrange
        let start = Instant::now();
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        tk.vibrate(Speed::new(50), TkDuration::from_secs(1), vec![]);
        thread::sleep(Duration::from_millis(500));
        tk.vibrate(Speed::new(100), TkDuration::from_millis(10), vec![]);
        thread::sleep(Duration::from_secs(1));

        // assert
        print_device_calls( &call_registry, 1, start );

        assert!(call_registry.get_device(1)[0].vibration_started_strength(0.5));
        assert!(call_registry.get_device(1)[1].vibration_started_strength(1.0));
        assert!(call_registry.get_device(1)[2].vibration_started_strength(0.5));
        assert!(call_registry.get_device(1)[3].vibration_stopped());
        assert_eq!(call_registry.get_device(1).len(), 4);
    }

    #[test]
    fn linear_correct_priority_3() {
        // call1  |111111111111111111111111111-->|
        // call2       |22222222222222->|
        // call3            |333->|
        // result |111122222333332222222111111-->|

        // arrange
        let start = Instant::now();
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        tk.vibrate(Speed::new(20), TkDuration::from_secs(3), vec![]);
        thread::sleep(Duration::from_millis(250));
        tk.vibrate(Speed::new(40), TkDuration::from_secs(2), vec![]);
        thread::sleep(Duration::from_millis(250));
        tk.vibrate(Speed::new(80), TkDuration::from_secs(1), vec![]);

        thread::sleep(Duration::from_secs(3));

        // assert
        print_device_calls( &call_registry, 1, start );

        assert!(call_registry.get_device(1)[0].vibration_started_strength(0.2));
        assert!(call_registry.get_device(1)[1].vibration_started_strength(0.4));
        assert!(call_registry.get_device(1)[2].vibration_started_strength(0.8));
        assert!(call_registry.get_device(1)[3].vibration_started_strength(0.4));
        assert!(call_registry.get_device(1)[4].vibration_started_strength(0.2));
        assert!(call_registry.get_device(1)[5].vibration_stopped());
        assert_eq!(call_registry.get_device(1).len(), 6);
    }

    #[test]
    fn linear_correct_priority_4() {
        // call1  |111111111111111111111111111-->|
        // call2       |22222222222->|
        // call3            |333333333-->|
        // result |111122222222222233333331111-->|

        // arrange
        let start = Instant::now();
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        tk.vibrate(Speed::new(20), TkDuration::from_secs(3), vec![]);
        thread::sleep(Duration::from_millis(250));
        tk.vibrate(Speed::new(40), TkDuration::from_secs(1), vec![]);
        thread::sleep(Duration::from_millis(250));
        tk.vibrate(Speed::new(80), TkDuration::from_secs(2), vec![]);
        thread::sleep(Duration::from_secs(3));

        // assert
        print_device_calls( &call_registry, 1, start );

        assert!(call_registry.get_device(1)[0].vibration_started_strength(0.2));
        assert!(call_registry.get_device(1)[1].vibration_started_strength(0.4));
        assert!(call_registry.get_device(1)[2].vibration_started_strength(0.8));
        assert!(call_registry.get_device(1)[3].vibration_started_strength(0.8));
        assert!(call_registry.get_device(1)[4].vibration_started_strength(0.2));
        assert!(call_registry.get_device(1)[5].vibration_stopped());
        assert_eq!(call_registry.get_device(1).len(), 6);
    }

    #[test]
    fn linear_overrides_pattern() {
        // lin1   |11111111111111111-->|
        // pat1       |23452345234523452345234-->|
        // result |1111111111111111111123452345234-->|

        // arrange
        let start = Instant::now();
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        let _lin1 = tk.vibrate(Speed::new(99), TkDuration::Infinite, vec![]);
        thread::sleep(Duration::from_millis(10));
        let _pat1 = tk.vibrate_pattern(TkPattern::Funscript(TkDuration::Infinite, String::from("31_Sawtooth-Fast")), vec![]);

        // assert
        print_device_calls( &call_registry, 1, start );

        thread::sleep(Duration::from_secs(2));
        tk.stop(_lin1);
        thread::sleep(Duration::from_secs(2));
        assert!(call_registry.get_device(1).len() > 3);
    }

    #[test]
    fn settings_only_vibrate_enabled_devices() {
        // arrange
        let (mut tk, call_registry) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Vibrate),
            scalar(3, "vib3", ActuatorType::Vibrate),
        ]);
        tk.settings_set_enabled("vib2", false);

        // act
        tk.vibrate(Speed::max(), TkDuration::from_millis(1), vec![]);
        thread::sleep(Duration::from_secs(1));

        // assert
        call_registry.assert_vibrated(1);
        call_registry.assert_vibrated(3);
        call_registry.assert_not_vibrated(2);
    }

    #[test]
    fn events_get() {
        let empty: Vec<String> = vec![];
        let one_event: Vec<String> = vec![String::from("evt2")];
        let two_events: Vec<String> = vec![String::from("evt2"), String::from("evt3")];

        let (mut tk, _) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Vibrate),
            scalar(3, "vib3", ActuatorType::Vibrate),
        ]);

        tk.settings_set_events("vib2", one_event.clone());
        tk.settings_set_events("vib3", two_events.clone());

        assert_eq!(tk.settings_get_events("vib1"), empty);
        assert_eq!(tk.settings_get_events("vib2"), one_event);
        assert_eq!(tk.settings_get_events("vib3"), two_events);
    }

    #[test]
    fn event_only_vibrate_selected_devices() {
        let (mut tk, call_registry) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Vibrate),
        ]);
        tk.settings_set_events("vib1", vec![String::from("selected_event")]);
        tk.settings_set_events("vib2", vec![String::from("bogus")]);

        tk.vibrate(
            Speed::max(),
            TkDuration::from_millis(1),
            vec![String::from("selected_event")],
        );
        thread::sleep(Duration::from_secs(1));

        call_registry.assert_vibrated(1);
        call_registry.assert_not_vibrated(2);
    }

    #[test]
    fn event_is_trimmed_and_ignores_casing() {
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);
        tk.settings_set_enabled("vib1", true);
        tk.settings_set_events("vib1", vec![String::from("some event")]);
        tk.vibrate(
            Speed::max(),
            TkDuration::from_millis(1),
            vec![String::from(" SoMe EvEnT    ")],
        );

        call_registry.assert_vibrated(1);
    }

    #[test]
    fn settings_are_trimmed_and_lowercased() {
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);
        tk.settings_set_enabled("vib1", true);
        tk.settings_set_events("vib1", vec![String::from(" SoMe EvEnT    ")]);
        tk.vibrate(
            Speed::max(),
            TkDuration::from_millis(1),
            vec![String::from("some event")],
        );

        call_registry.assert_vibrated(1);
    }

    #[test]
    fn get_device_capabilities() {
        // arrange
        let (tk, _) = wait_for_connection(vec![
            scalar(1, "vib1", ActuatorType::Vibrate),
            scalar(2, "vib2", ActuatorType::Constrict),
            linear(3, "lin2"),
        ]);

        // assert
        assert!(
            tk.get_device_capabilities("not exist").is_empty(),
            "Non existing device returns empty list"
        );
        assert!(
            tk.get_device_capabilities("vib2").is_empty(),
            "Unsupported capability is not returned"
        );
        assert!(
            tk.get_device_capabilities("lin2").is_empty(),
            "Unsupported capability is not returned"
        );
        assert_eq!(
            tk.get_device_capabilities("vib1").first().unwrap(),
            &String::from("Vibrate"),
            "vibrator returns vibrate"
        );
    }

    #[test]
    fn get_device_connected() {
        let (tk, _) = wait_for_connection(vec![scalar(1, "existing", ActuatorType::Vibrate)]);
        assert!(
            tk.get_device_connected("existing"),
            "Existing device returns true"
        );
        assert_eq!(
            tk.get_device_connected("not existing"),
            false,
            "Non-existing device returns false"
        );
    }

    #[test]
    fn vibrate_infinitely_and_then_stop() {
        // arrange
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        let handle = tk.vibrate(Speed::max(), TkDuration::Infinite, vec![]);
        thread::sleep(Duration::from_secs(1));
        call_registry.assert_started(1);

        tk.stop(handle);
        call_registry.assert_vibrated(1);
    }

    #[test]
    fn vibrate_linear_then_cancel() {
        // arrange
        let (mut tk, call_registry) =
            wait_for_connection(vec![scalar(1, "vib1", ActuatorType::Vibrate)]);

        // act
        tk.vibrate(
            Speed::max(),
            TkDuration::Timed(Duration::from_secs(10)),
            vec![],
        );
        thread::sleep(Duration::from_secs(1));
        call_registry.assert_started(1);

        tk.stop_all();
        call_registry.assert_vibrated(1);
    }

    fn wait_for_connection(devices: Vec<DeviceAdded>) -> (Telekinesis, FakeConnectorCallRegistry) {
        let devices_names: Vec<String> = devices.iter().map(|d| d.device_name().clone()).collect();
        let (connector, call_registry) = FakeDeviceConnector::new(devices);
        let count = connector.devices.len();

        // act
        let mut settings = TkSettings::default();
        settings.pattern_path = String::from("../contrib/Distribution/SKSE/Plugins/Telekinesis/Patterns");
        let mut tk = Telekinesis::connect_with(|| async move { connector }, Some(settings)).unwrap();
        tk.await_connect(count);

        for name in devices_names {
            tk.settings_set_enabled(&name, true);
        }

        (tk, call_registry)
    }

    #[test]
    #[ignore = "Requires one (1) vibrator to be connected via BTLE (vibrates it)"]
    fn vibrate_pattern_then_cancel() {
        let mut settings = TkSettings::default();
        settings.pattern_path = String::from("../contrib/Distribution/SKSE/Plugins/Telekinesis/Patterns");

        let mut tk =
            Telekinesis::connect_with(|| async move { in_process_connector() }, Some(settings)).unwrap();
        tk.scan_for_devices();
        tk.await_connect(1);
        thread::sleep(Duration::from_secs(2));
        tk.settings
            .set_enabled(tk.get_device_names().first().unwrap(), true);

        enable_log();
        let handle = tk.vibrate_pattern(
            TkPattern::Funscript(TkDuration::from_secs(10), String::from("02_Cruel-Tease")),
            vec![],
        );
        thread::sleep(Duration::from_secs(2)); // dont disconnect
        tk.stop(handle);
        thread::sleep(Duration::from_secs(10));
    }

    #[test]
    #[ignore = "Requires one (1) vibrator to be connected via BTLE (vibrates it)"]
    fn test_funscript_vibrate_10s() { // TODO: Does not assert if the vibration actually happened
        enable_log();

        let mut tk =
            Telekinesis::connect_with(|| async move { in_process_connector() }, None).unwrap();
        tk.scan_for_devices();
        tk.await_connect(1);
        thread::sleep(Duration::from_secs(2));
        let _ = tk.process_next_events();
        assert!( matches!( tk.connection_status, TkConnectionStatus::Connected));

        tk.settings
            .set_enabled(tk.get_device_names().first().unwrap(), true);

        tk.vibrate_pattern(
            TkPattern::Funscript(TkDuration::from_secs(10), String::from("Tease_30s")),
            vec![],
        );
        thread::sleep(Duration::from_secs(15)); // dont disconnect
        tk.stop_all();
    }

    #[test]
    #[ignore = "Requires intiface to be connected, with a connected device (vibrates it)"]
    fn intiface_test_vibration() {
        enable_log();

        let mut settings = TkSettings::default();
        settings.connection = TkConnectionType::WebSocket(String::from("127.0.0.1:12345"));

        let mut tk = Telekinesis::connect(settings).unwrap();
        tk.scan_for_devices();

        thread::sleep(Duration::from_secs(5));
        let _ = tk.process_next_events();
        assert!( matches!( tk.connection_status, TkConnectionStatus::Connected));

        for device in tk.get_devices() {
            tk.settings.set_enabled(device.name(), true);
        }
        tk.vibrate(Speed::max(), TkDuration::Infinite, vec![]);
        thread::sleep(Duration::from_secs(5));
    }
 
    #[test]
    fn intiface_not_available_connection_status_error() {
        let mut settings = TkSettings::default();
        settings.connection = TkConnectionType::WebSocket(String::from("bogushost:6572"));

        let mut tk = Telekinesis::connect(settings).unwrap();
        tk.scan_for_devices();
        thread::sleep(Duration::from_secs(5));
        let _ = tk.process_next_events();

        match tk.connection_status {
            TkConnectionStatus::Failed(err) => {
                assert!(err.len() > 0);
            },
            _ => todo!()
        }
    }

    fn print_device_calls( call_registry: &FakeConnectorCallRegistry, index: u32, test_start: Instant ) {
        for i in 0..call_registry.get_device(index).len() {
            let fake_call = call_registry.get_device(1)[i].clone();
            let s = fake_call.get_scalar_strength();
            let t = (test_start.elapsed() - fake_call.time.elapsed()).as_millis();
            let perc = (s * 100.0).round();
            println!("{:02} @{:04} ms {percent:>3}% {empty:=>width$}", i, t, percent=perc as i32, empty = "", width = (perc/5.0).floor() as usize);
        }
    }
}

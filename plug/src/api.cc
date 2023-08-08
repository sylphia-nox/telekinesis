
#include "plug/src/lib.rs.h"

#include <vector>
#include <string>

#define DllExport __declspec(dllexport)

namespace Tk
{
    // SKSE plugin loading requires us to declare all papyrus calls as dllexports
    // otherwise they won't be found
    DllExport bool Connect(void*) {
        return tk_connect();
    }
    DllExport bool ScanForDevices(void*) {
        return tk_scan_for_devices();
    }
    DllExport bool StopScan(void*) {
        return tk_stop_scan();
    }
    DllExport bool Close(void*) {
        return tk_close();
    }
    DllExport std::vector<std::string> GetDevices(void*) {
        auto names = tk_get_devices();
        return std::vector<std::string>(names.begin(), names.end());
    }
    DllExport std::vector<std::string> GetDeviceCapabilities(void*, std::string device_name) {
        auto names = tk_get_device_capabilities(device_name);
        return std::vector<std::string>(names.begin(), names.end());
    }
    DllExport bool GetDeviceConnected(void*, std::string device_name) {
        return tk_get_device_connected(device_name);
    }
    DllExport bool Vibrate(void*, int speed, float time_sec) {
        return tk_vibrate(speed, time_sec);
    }
    DllExport bool VibrateEvents(void*, int speed, float time_sec, std::vector<std::string> events) {
        return tk_vibrate_events(speed, time_sec, events);
    }
    DllExport bool StopAll(void*) {
        return tk_stop_all(); 
    }
    DllExport std::vector<std::string> PollEvents(void*) {
        auto events = tk_poll_events();
        return std::vector<std::string>(events.begin(), events.end());
    }
    DllExport bool GetEnabled(void*, std::string device_name) {
        return tk_settings_get_enabled(device_name);
    }
    DllExport void SetEnabled(void*, std::string device_name, bool enabled) {
        tk_settings_set_enabled(device_name, enabled);
    }
    DllExport bool SettingsStore(void*) {
        return tk_settings_store();
    }
}

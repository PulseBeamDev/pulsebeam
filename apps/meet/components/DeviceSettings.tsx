import { MenuItem, TextField } from "@mui/material";
import { useMeetMedia } from "./MeetMediaProvider";

export function DeviceSettings() {
  const {
    devices,
    videoDeviceId,
    audioDeviceId,
    setVideoDeviceId,
    setAudioDeviceId,
  } = useMeetMedia();
  const fields = [
    ["Camera", videoDeviceId, devices.cameras, setVideoDeviceId],
    ["Microphone", audioDeviceId, devices.microphones, setAudioDeviceId],
  ] as const;
  return fields.map(([label, value, choices, change]) => (
    <TextField
      key={label}
      select
      size="small"
      fullWidth
      label={label}
      value={value}
      onChange={(event) => change(event.target.value)}
      disabled={!choices.length}
    >
      {(!choices.length || !value) && (
        <MenuItem value="">
          {choices.length ? `Select ${label}…` : "No devices found"}
        </MenuItem>
      )}
      {choices.map((device) => (
        <MenuItem key={device.id} value={device.id}>
          {device.label}
        </MenuItem>
      ))}
    </TextField>
  ));
}

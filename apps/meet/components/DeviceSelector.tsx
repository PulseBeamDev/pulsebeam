interface DeviceSelectorProps {
  label: string;
  value: string;
  devices: MediaDeviceInfo[];
  onValueChange: (deviceId: string) => void;
}

export function DeviceSelector({
  label,
  value,
  devices,
  onValueChange,
}: DeviceSelectorProps) {
  return (
    <div className="space-y-2">
      <label className="block text-xs font-semibold tracking-wider text-muted-foreground uppercase">
        {label}
        <select
          className="mt-2 h-11 w-full rounded-md border border-border bg-background px-2.5 text-sm font-normal tracking-normal normal-case shadow-xs outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:opacity-50 sm:h-9"
          value={value}
          onChange={(event) => onValueChange(event.target.value)}
          disabled={devices.length === 0}
        >
          {devices.length === 0 ? (
            <option value="">No devices found</option>
          ) : (
            <>
              {!value && <option value="">Select {label}...</option>}
              {devices.map((device) => (
                <option key={device.deviceId} value={device.deviceId}>
                  {device.label}
                </option>
              ))}
            </>
          )}
        </select>
      </label>
    </div>
  );
}

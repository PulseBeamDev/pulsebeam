import { cn } from "./ui";
import { useEffect, useRef } from "react";

export function LocalVideo({ stream, mirror = false, ...props }: { stream: MediaStream; mirror?: boolean } & React.VideoHTMLAttributes<HTMLVideoElement>) {
  const ref = useRef<HTMLVideoElement>(null);

  useEffect(() => {
    if (ref.current && stream) ref.current.srcObject = stream;
  }, [stream]);

  return <video ref={ref} autoPlay muted playsInline {...props} className={cn(props.className, mirror && "mirror")} />;
}


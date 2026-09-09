import { Button } from "./ui";
import { Mic, MicOff, Video as VideoIcon, VideoOff } from "lucide-react";
import { RefObject } from "react";

interface MediaPreviewProps {
  videoRef: RefObject<HTMLVideoElement | null>;
  isCamOn: boolean;
  isMicOn: boolean;
  onToggleCam: () => void;
  onToggleMic: () => void;
  hasStream: boolean;
}

export function MediaPreview({
  videoRef,
  isCamOn,
  isMicOn,
  onToggleCam,
  onToggleMic,
  hasStream
}: MediaPreviewProps) {
  return (
    <div className="space-y-4">
      <div className="relative aspect-video overflow-hidden rounded-lg border border-border bg-black shadow-inner">
        {hasStream && isCamOn ? (
          <video
            ref={videoRef}
            autoPlay
            muted
            playsInline
            className="w-full h-full object-contain mirror"
          />
        ) : (
          <div className="flex flex-col items-center justify-center h-full bg-muted text-muted-foreground transition-colors">
            <div className="p-4 rounded-full bg-background/50 mb-2">
              <VideoOff className="w-8 h-8 opacity-20" />
            </div>
            <span className="text-sm font-medium">
              {!hasStream ? "Initializing hardware..." : "Camera is turned off"}
            </span>
          </div>
        )}

        {/* Floating Controls Overlay */}
        <div className="absolute bottom-3 left-1/2 z-10 flex -translate-x-1/2 gap-3 sm:bottom-4">
          <Button
            type="button" // Important: prevents form submission
            size="icon"
            variant={isMicOn ? "secondary" : "destructive"}
            className="h-11 w-11 rounded-full shadow-lg transition-transform hover:scale-105 sm:h-12 sm:w-12"
            onClick={onToggleMic}
          >
            {isMicOn ? <Mic className="w-5 h-5" /> : <MicOff className="w-5 h-5" />}
          </Button>

          <Button
            type="button"
            size="icon"
            variant={isCamOn ? "secondary" : "destructive"}
            className="h-11 w-11 rounded-full shadow-lg transition-transform hover:scale-105 sm:h-12 sm:w-12"
            onClick={onToggleCam}
          >
            {isCamOn ? <VideoIcon className="w-5 h-5" /> : <VideoOff className="w-5 h-5" />}
          </Button>
        </div>
      </div>
    </div>
  );
}


"use client";

import { useCallback, useState } from "react";
import { CssBaseline, ThemeProvider, createTheme } from "@mui/material";
import { Lobby } from "./Lobby";
import { MeetMediaProvider, useMeetMedia } from "./MeetMediaProvider";
import { Room } from "./Room";

const theme = createTheme({
  palette: {
    primary: { main: "#2858bd" },
    error: { main: "#b32637" },
    background: { default: "#f5f7fb", paper: "#ffffff" },
  },
  typography: { fontFamily: "Manrope, system-ui, sans-serif" },
  shape: { borderRadius: 10 },
  components: {
    MuiButton: { defaultProps: { disableElevation: true } },
  },
});

export function MeetApp() {
  return (
    <ThemeProvider theme={theme}>
      <CssBaseline />
      <MeetMediaProvider>
        <MeetSession />
      </MeetMediaProvider>
    </ThemeProvider>
  );
}

function MeetSession() {
  const [session, setSession] = useState<{
    token: string;
    endpoint: string;
    cameraOn: boolean;
    micOn: boolean;
  } | null>(null);
  const { stop } = useMeetMedia().capture;
  const leave = useCallback(() => {
    stop();
    setSession(null);
  }, [stop]);
  return session ? (
    <Room {...session} onLeave={leave} />
  ) : (
    <Lobby
      onJoin={(token, endpoint, cameraOn, micOn) =>
        setSession({ token, endpoint, cameraOn, micOn })
      }
    />
  );
}

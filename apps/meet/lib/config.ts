const localServerUrl = "http://localhost:7070";
const productionServerUrl = "https://demo.pulsebeam.dev";

export const defaultServerUrl =
  process.env.NODE_ENV === "development"
    ? (process.env.NEXT_PUBLIC_PULSEBEAM_SERVER_URL ?? localServerUrl)
    : productionServerUrl;

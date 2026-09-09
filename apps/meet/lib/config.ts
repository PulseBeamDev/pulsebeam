const localApiUrl = "http://localhost:7070/api/v1";
const productionApiUrl = "https://demo.pulsebeam.dev/api/v1";

export const defaultApiUrl =
  process.env.NODE_ENV === "development"
    ? (process.env.NEXT_PUBLIC_PULSEBEAM_API_URL ?? localApiUrl)
    : productionApiUrl;

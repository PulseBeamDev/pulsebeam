const localApiUrl = "http://localhost:7070/api/v1";

export const defaultApiUrl =
  process.env.NEXT_PUBLIC_PULSEBEAM_API_URL ??
  (process.env.NODE_ENV === "development" ? localApiUrl : "");

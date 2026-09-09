(async () => JSON.stringify(await (__PULSEBEAM_EXPRESSION__), (_key, value) => typeof value === "bigint" ? value.toString() : value))()

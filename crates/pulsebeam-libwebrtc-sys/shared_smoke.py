"""Load and exercise the existing real-RTC shared-library consumer."""

import ctypes
import sys

library = ctypes.CDLL(sys.argv[1])
assert library.pulsebeam_dynamic_smoke() == 0

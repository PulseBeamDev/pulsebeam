#!/usr/bin/env python3
"""Preserve the codec/ANA floating-point contract after target-specific flags."""
import os
import sys

os.execv(sys.argv[1], sys.argv[1:] + ["-fno-fast-math", "-ffp-contract=off"])

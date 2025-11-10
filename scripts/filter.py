import pandas as pd
import io
import sys
import math


# --- Sigmoid Function for Quality Scoring ---
# This function maps the Kalman filter output (m_hat) to a 0-100 score.
# - A low m_hat (low queueing pressure) results in a high score.
# - A high m_hat (high queueing pressure) results in a low score.
def sigmoid(value, range_max, k, midpoint):
    """
    Calculates a sigmoid function. Used here as a descending curve.
    - value: The input value (in this case, abs(m_hat)).
    - range_max: The max value of the score (e.g., 100).
    - k: The steepness of the curve. A negative value makes it descending.
    - midpoint: The center of the curve. This should be your "overuse" threshold.
    """
    try:
        return range_max / (1.0 + math.exp(-k * (value - midpoint)))
    except OverflowError:
        return 0


# --- Main Script ---

# Read the data from standard input into a pandas DataFrame
df = pd.read_csv(sys.stdin)

# --- Kalman Filter Initialization ---
# These values are based on the recommendations in the GCC paper (draft-ietf-rmcat-gcc-02)

# State variables (will be updated in the loop)
m_hat = 0.0  # Estimate of the queue delay trend, m_hat(i). Starts at 0.
e = 0.1  # Variance of the estimate, e(i). Initial value from paper's Table 1.
var_v_hat = 1.0  # Estimated measurement noise variance. Clamped at >= 1.

# Filter parameters (constants)
Q = 1e-3  # State noise covariance (q in the paper). Recommended value.
CHI = 0.01  # Filter coefficient for var_v_hat. Recommended range [0.1, 0.001].

# The alpha for the var_v_hat EWMA is complex in the paper as it depends on packet rate (f_max).
# We can use a fixed version based on a typical frame rate (e.g., 30fps) for simplicity.
# alpha = (1-chi)^(30/(1000 * f_max))
# Assuming f_max = 30fps for this calculation.
ALPHA = (1.0 - CHI) ** (30.0 / (1000.0 * 30.0))

# --- Scoring Function Parameters ---
# These control how m_hat is translated into a quality score.
SCORE_K = -0.3  # Steepness of the quality curve.
SCORE_MIDPOINT = 12.5  # The m_hat value corresponding to a score of 50. From GCC paper (del_var_th(0)).

# Lists to store the calculated values for each row
m_hat_list = []
quality_score_list = []

# --- Process Data Row-by-Row ---
# We loop because each Kalman step depends on the result of the previous one.
for row in df.itertuples():
    # The 'dod' column in your data is the inter-group delay variation, d(i) in the paper.
    d_i = row.dod

    # --- Kalman Filter Update Equations (from GCC paper, Section 5.3) ---

    # 1. Calculate Kalman gain, k(i)
    k = (e + Q) / (var_v_hat + e + Q)

    # 2. Update the estimate, m_hat(i)
    # z(i) = d(i) - m_hat(i-1)
    z = d_i - m_hat
    m_hat = m_hat + z * k

    # 3. Update the estimate variance, e(i)
    e = (1.0 - k) * (e + Q)

    # 4. Update the measurement noise variance, var_v_hat
    # The paper uses an outlier filter, clamping z to 3 standard deviations.
    z_clamped = (
        abs(z) if abs(z) < 3.0 * math.sqrt(var_v_hat) else 3.0 * math.sqrt(var_v_hat)
    )
    var_v_hat = max(1.0, ALPHA * var_v_hat + (1.0 - ALPHA) * (z_clamped**2))

    # --- Calculate Quality Score ---
    # The score is based on the absolute value of our clean m_hat estimate.
    score = sigmoid(abs(m_hat), 100.0, SCORE_K, SCORE_MIDPOINT)

    # Append results to our lists
    m_hat_list.append(m_hat)
    quality_score_list.append(score)


# Add the new calculated columns to the DataFrame
df["m_hat"] = m_hat_list
df["quality_score"] = quality_score_list

# Print the final DataFrame with the new columns
print(df.to_csv(index=False))

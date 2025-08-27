# PulseBeam Actor

## Objective

* High throughput and low contention
* Fault isolation
* Easy to reason about concurrency
* Control over async runtime and channel
* Testability


## Non-objective

* Pure actor indirect messaging or global addresses

## Todo

* Use multiple SPSC channels as opposed to MPSC channels to reduce contention, https://github.com/zesterer/flume/issues/152

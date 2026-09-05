/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

#ifndef GUACD_METRICS_H
#define GUACD_METRICS_H

/**
 * Starts the Prometheus metrics endpoint if GUACD_METRICS_PORT is set:
 * an HTTP listener (GUACD_METRICS_BIND, default 0.0.0.0) serving /metrics
 * and /health. Connection processes write their video metrics to files in
 * GUACD_METRICS_DIR (set here for them; default /tmp/guacd-metrics), which
 * /metrics combines. Must be called in the final (daemonized) process,
 * before connections are accepted.
 */
void guacd_metrics_start(void);

#endif

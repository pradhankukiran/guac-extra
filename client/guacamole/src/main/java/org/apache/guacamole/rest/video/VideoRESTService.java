/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

package org.apache.guacamole.rest.video;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import javax.inject.Inject;
import javax.ws.rs.GET;
import javax.ws.rs.Path;
import javax.ws.rs.Produces;
import javax.ws.rs.core.MediaType;
import org.apache.guacamole.GuacamoleException;
import org.apache.guacamole.GuacamoleSecurityException;
import org.apache.guacamole.GuacamoleSession;
import org.apache.guacamole.environment.Environment;
import org.apache.guacamole.net.auth.UserContext;
import org.apache.guacamole.net.auth.permission.SystemPermission;
import org.apache.guacamole.properties.StringGuacamoleProperty;
import org.apache.guacamole.rest.TokenParam;
import org.apache.guacamole.rest.auth.AuthenticationService;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * guac-extra: live video statistics of every session, for administrators.
 * Reads guacd's Prometheus endpoint (see guac-extra docs/METRICS.md) and
 * returns its per-session samples as JSON for the "Video" settings page.
 */
@Path("/video")
@Produces(MediaType.APPLICATION_JSON)
public class VideoRESTService {

    /**
     * Logger for this class.
     */
    private static final Logger logger = LoggerFactory.getLogger(VideoRESTService.class);

    /**
     * URL of guacd's metrics endpoint. Default:
     * http://&lt;guacd-hostname&gt;:9100/metrics.
     */
    public static final StringGuacamoleProperty GUACD_METRICS_URL = new StringGuacamoleProperty() {

        @Override
        public String getName() { return "guacd-metrics-url"; }

    };

    /**
     * How long to wait for guacd's metrics endpoint, in milliseconds.
     */
    private static final int TIMEOUT_MS = 2000;

    /**
     * One Prometheus sample: name{labels} value.
     */
    private static final Pattern SAMPLE = Pattern.compile(
            "^([a-z_]+)\\{([^}]*)\\}\\s+([-0-9.eE+]+)$");

    /**
     * One label within a sample's braces.
     */
    private static final Pattern LABEL = Pattern.compile("([a-z_]+)=\"([^\"]*)\"");

    /**
     * Service for authenticating users from auth tokens.
     */
    @Inject
    private AuthenticationService authenticationService;

    /**
     * The Guacamole server environment.
     */
    @Inject
    private Environment environment;

    /**
     * The statistics returned to the browser.
     */
    public static class VideoStatistics {

        /**
         * Whether guacd's metrics endpoint answered.
         */
        public boolean available;

        /**
         * Why it didn't, if it didn't.
         */
        public String error;

        /**
         * When the samples were read (milliseconds since the epoch), so the
         * browser can turn counters into rates.
         */
        public long time;

        /**
         * The sessions guacd reports, one map per connection: the
         * "connection" ID plus each metric (names without the
         * "guac_extra_video_" prefix; users by kind as "users_video",
         * "users_images" and so on).
         */
        public List<Map<String, Object>> sessions = new ArrayList<>();

    }

    /**
     * Returns the live video statistics of every session. Administrators
     * only.
     *
     * @param authToken
     *     The authentication token of the requesting user.
     *
     * @return
     *     The statistics, with "available" false if guacd's metrics endpoint
     *     could not be read (and the reason in "error").
     *
     * @throws GuacamoleException
     *     If the token is invalid or the user is not an administrator.
     */
    @GET
    @Path("/sessions")
    public VideoStatistics getSessions(@TokenParam String authToken)
            throws GuacamoleException {

        GuacamoleSession session = authenticationService.getGuacamoleSession(authToken);

        boolean administrator = false;
        for (UserContext context : session.getUserContexts()) {
            if (context.self().getEffectivePermissions().getSystemPermissions()
                    .hasPermission(SystemPermission.Type.ADMINISTER)) {
                administrator = true;
                break;
            }
        }

        if (!administrator)
            throw new GuacamoleSecurityException("Permission to view video statistics denied.");

        String url = environment.getProperty(GUACD_METRICS_URL);
        if (url == null)
            url = "http://" + environment.getProperty(Environment.GUACD_HOSTNAME, "localhost")
                    + ":9100/metrics";

        VideoStatistics statistics = new VideoStatistics();
        statistics.time = System.currentTimeMillis();

        try {
            statistics.sessions = parse(fetch(url));
            statistics.available = true;
        }
        catch (IOException e) {
            statistics.error = "guacd metrics at " + url + " could not be read: " + e.getMessage();
            logger.debug("Reading guacd metrics failed.", e);
        }

        return statistics;

    }

    /**
     * Reads the text at the given URL.
     *
     * @param address
     *     The URL.
     *
     * @return
     *     The response body.
     *
     * @throws IOException
     *     If the URL can't be read.
     */
    private static String fetch(String address) throws IOException {

        HttpURLConnection connection = (HttpURLConnection) new URL(address).openConnection();
        connection.setConnectTimeout(TIMEOUT_MS);
        connection.setReadTimeout(TIMEOUT_MS);

        try {
            int status = connection.getResponseCode();
            if (status != HttpURLConnection.HTTP_OK)
                throw new IOException("HTTP " + status);

            StringBuilder body = new StringBuilder();
            try (BufferedReader reader = new BufferedReader(new InputStreamReader(
                    connection.getInputStream(), StandardCharsets.UTF_8))) {
                String line;
                while ((line = reader.readLine()) != null)
                    body.append(line).append('\n');
            }
            return body.toString();
        }
        finally {
            connection.disconnect();
        }

    }

    /**
     * Groups guac-extra's per-session samples by connection.
     *
     * @param text
     *     Prometheus text exposition from guacd.
     *
     * @return
     *     One map per connection.
     */
    static List<Map<String, Object>> parse(String text) {

        Map<String, Map<String, Object>> byConnection = new TreeMap<>();

        for (String line : text.split("\n")) {

            Matcher sample = SAMPLE.matcher(line.trim());
            if (!sample.matches() || !sample.group(1).startsWith("guac_extra_video_"))
                continue;

            String name = sample.group(1).substring("guac_extra_video_".length());
            String connection = null;
            String kind = null;

            Matcher label = LABEL.matcher(sample.group(2));
            while (label.find()) {
                if (label.group(1).equals("connection"))
                    connection = label.group(2);
                else if (label.group(1).equals("kind"))
                    kind = label.group(2);
            }

            if (connection == null)
                continue;

            Map<String, Object> values = byConnection.computeIfAbsent(connection, id -> {
                Map<String, Object> map = new TreeMap<>();
                map.put("connection", id);
                return map;
            });

            double value;
            try {
                value = Double.parseDouble(sample.group(3));
            }
            catch (NumberFormatException e) {
                continue;
            }

            values.put(kind != null ? name + "_" + kind : name, value);

        }

        return new ArrayList<>(byConnection.values());

    }

}

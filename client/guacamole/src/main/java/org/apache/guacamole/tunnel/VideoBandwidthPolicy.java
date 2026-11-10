/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

package org.apache.guacamole.tunnel;

import java.util.Collection;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import javax.inject.Inject;
import javax.inject.Singleton;
import org.apache.guacamole.GuacamoleException;
import org.apache.guacamole.GuacamoleSession;
import org.apache.guacamole.environment.Environment;
import org.apache.guacamole.net.auth.UserContext;
import org.apache.guacamole.properties.StringGuacamoleProperty;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * guac-extra: per-user and per-group video bitrate caps.
 *
 * The "video-bandwidth-policy" property holds comma-separated rules:
 *
 * <pre>
 * video-bandwidth-policy: group:students=2000, group:staff=10000, user:alice=5000, default=4000
 * </pre>
 *
 * A user's own rule wins; otherwise the highest rule among the groups the
 * user belongs to; otherwise "default". The result (kbit/s) is offered to
 * every connection as the parameter token ${GUAC_EXTRA_MAX_KBPS} (empty if
 * no rule applies), so a connection with video-max-kbps set to
 * ${GUAC_EXTRA_MAX_KBPS} gets each user's own cap.
 */
@Singleton
public class VideoBandwidthPolicy {

    /**
     * Logger for this class.
     */
    private static final Logger logger = LoggerFactory.getLogger(VideoBandwidthPolicy.class);

    /**
     * The token each connection receives.
     */
    public static final String TOKEN = "GUAC_EXTRA_MAX_KBPS";

    /**
     * The property holding the rules.
     */
    public static final StringGuacamoleProperty VIDEO_BANDWIDTH_POLICY = new StringGuacamoleProperty() {

        @Override
        public String getName() { return "video-bandwidth-policy"; }

    };

    /**
     * The Guacamole server environment.
     */
    @Inject
    private Environment environment;

    /**
     * Parsed rules of a policy.
     */
    static class Rules {

        /**
         * Caps by username.
         */
        final Map<String, Integer> users = new HashMap<>();

        /**
         * Caps by user group identifier.
         */
        final Map<String, Integer> groups = new HashMap<>();

        /**
         * Cap when no other rule applies, or null.
         */
        Integer fallback;

    }

    /**
     * Parses the rules of a policy. Malformed rules are logged and skipped.
     *
     * @param policy
     *     The property's value.
     *
     * @return
     *     The rules.
     */
    static Rules parse(String policy) {

        Rules rules = new Rules();
        if (policy == null)
            return rules;

        for (String rule : policy.split(",")) {

            rule = rule.trim();
            if (rule.isEmpty())
                continue;

            int equals = rule.lastIndexOf('=');
            if (equals <= 0) {
                logger.warn("Ignoring video bandwidth rule \"{}\": expected NAME=KBPS.", rule);
                continue;
            }

            String subject = rule.substring(0, equals).trim();
            int kbps;
            try {
                kbps = Integer.parseInt(rule.substring(equals + 1).trim());
            }
            catch (NumberFormatException e) {
                logger.warn("Ignoring video bandwidth rule \"{}\": the limit is not a number.", rule);
                continue;
            }

            if (kbps <= 0) {
                logger.warn("Ignoring video bandwidth rule \"{}\": the limit must be positive.", rule);
                continue;
            }

            if (subject.equals("default"))
                rules.fallback = kbps;
            else if (subject.startsWith("user:"))
                rules.users.put(subject.substring(5), kbps);
            else if (subject.startsWith("group:"))
                rules.groups.put(subject.substring(6), kbps);
            else
                logger.warn("Ignoring video bandwidth rule \"{}\": expected user:, "
                        + "group: or default.", rule);

        }

        return rules;

    }

    /**
     * Applies rules to a user.
     *
     * @param rules
     *     The rules.
     *
     * @param username
     *     The user's name.
     *
     * @param groups
     *     Identifiers of every group the user belongs to.
     *
     * @return
     *     The cap in kbit/s, or null if no rule applies.
     */
    static Integer apply(Rules rules, String username, Collection<String> groups) {

        if (username != null && rules.users.containsKey(username))
            return rules.users.get(username);

        Integer best = null;
        for (String group : groups) {
            Integer kbps = rules.groups.get(group);
            if (kbps != null && (best == null || kbps > best))
                best = kbps;
        }

        return best != null ? best : rules.fallback;

    }

    /**
     * Adds ${GUAC_EXTRA_MAX_KBPS} for the session's user to the given
     * connection tokens: the user's cap, or an empty string if no rule
     * applies (guacd then uses the connection's own setting or default).
     *
     * @param session
     *     The user's session.
     *
     * @param tokens
     *     The tokens the connection will receive.
     */
    public void addToken(GuacamoleSession session, Map<String, String> tokens) {

        Integer kbps = null;

        try {

            Rules rules = parse(environment.getProperty(VIDEO_BANDWIDTH_POLICY));

            Set<String> groups = new HashSet<>(session.getAuthenticatedUser().getEffectiveUserGroups());
            for (UserContext context : session.getUserContexts()) {
                try {
                    groups.addAll(context.self().getUserGroups().getObjects());
                }
                catch (GuacamoleException e) {
                    logger.debug("Group memberships of a user could not be read.", e);
                }
            }

            kbps = apply(rules, session.getAuthenticatedUser().getIdentifier(), groups);

        }
        catch (GuacamoleException e) {
            logger.warn("The video bandwidth policy could not be read: {}", e.getMessage());
            logger.debug("Reading the video bandwidth policy failed.", e);
        }

        tokens.put(TOKEN, kbps != null ? Integer.toString(kbps) : "");

    }

}

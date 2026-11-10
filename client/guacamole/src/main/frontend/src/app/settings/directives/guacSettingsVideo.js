/*
 * Licensed under the Apache License, Version 2.0 (the "License"); you may
 * not use this file except in compliance with the License. You may obtain a
 * copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Part of guac-extra. Not part of Apache Guacamole.
 */

/**
 * guac-extra: live video statistics of every session (administrators).
 * Polls api/video/sessions, which reads guacd's metrics endpoint, every two
 * seconds and turns its counters into rates.
 */
angular.module('settings').directive('guacSettingsVideo', [function guacSettingsVideo() {

    return {
        restrict: 'E',
        replace: true,
        scope: {},
        templateUrl: 'app/settings/templates/settingsVideo.html',
        controller: ['$scope', '$injector', function settingsVideoController($scope, $injector) {

            var $interval             = $injector.get('$interval');
            var authenticationService = $injector.get('authenticationService');

            /**
             * How often to refresh, in milliseconds.
             *
             * @type Number
             */
            var REFRESH_MS = 2000;

            /**
             * The previous response, for rates.
             *
             * @type Object
             */
            var previous = null;

            /**
             * Rows shown in the table, or null before the first response.
             *
             * @type Object[]
             */
            $scope.rows = null;

            /**
             * Why statistics are unavailable, if they are.
             *
             * @type String
             */
            $scope.error = null;

            /**
             * Totals across sessions.
             *
             * @type Object
             */
            $scope.totals = null;

            /**
             * Builds a table row from a session's samples and, if known, the
             * same session's samples from the previous response.
             *
             * @param {Object} now
             * @param {Object} before
             * @param {Number} seconds
             * @returns {Object}
             */
            var toRow = function toRow(now, before, seconds) {

                var rate = function rate(key) {
                    if (!before || seconds <= 0 || before[key] === undefined)
                        return null;
                    return Math.max(0, (now[key] - before[key]) / seconds);
                };

                var fps = rate('frames_total');
                var bytes = rate('bytes_total');

                return {
                    connection   : now.connection,
                    active       : now.active === 1,
                    passthrough  : now.passthrough === 1,
                    scale        : now.scale || 1,
                    usersVideo   : (now.users_video || 0),
                    usersLow     : (now.users_video_low || 0),
                    usersPaused  : (now.users_paused || 0),
                    usersImages  : (now.users_images || 0),
                    fps          : fps,
                    kbps         : bytes === null ? null : bytes * 8 / 1000,
                    targetKbps   : now.target_kbps || 0,
                    keyframes    : now.keyframes_total || 0,
                    fallbacks    : now.fallbacks_total || 0,
                    errors       : now.errors_total || 0
                };

            };

            /**
             * Fetches and shows the current statistics.
             */
            var refresh = function refresh() {
                authenticationService.request({
                    method : 'GET',
                    url    : 'api/video/sessions'
                })
                .then(function statisticsReceived(statistics) {

                    if (!statistics.available) {
                        $scope.error = statistics.error;
                        $scope.rows = [];
                        previous = null;
                        return;
                    }

                    $scope.error = null;

                    var before = {};
                    var seconds = 0;
                    if (previous) {
                        seconds = (statistics.time - previous.time) / 1000;
                        angular.forEach(previous.sessions, function index(s) {
                            before[s.connection] = s;
                        });
                    }

                    var rows = [];
                    var totals = { sessions: 0, video: 0, images: 0, low: 0, kbps: 0 };
                    angular.forEach(statistics.sessions, function addRow(s) {
                        var row = toRow(s, before[s.connection], seconds);
                        rows.push(row);
                        totals.sessions++;
                        totals.video += row.usersVideo;
                        totals.low += row.usersLow;
                        totals.images += row.usersImages;
                        totals.kbps += row.kbps || 0;
                    });

                    $scope.rows = rows;
                    $scope.totals = totals;
                    previous = statistics;

                }, function requestFailed(error) {
                    $scope.error = (error && error.message) || 'Request failed';
                });
            };

            refresh();
            var timer = $interval(refresh, REFRESH_MS);
            $scope.$on('$destroy', function stopRefreshing() {
                $interval.cancel(timer);
            });

        }]
    };

}]);

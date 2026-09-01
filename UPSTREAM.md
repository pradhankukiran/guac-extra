# Upstream sources

`server/` and `client/` are copies of Apache Guacamole, taken without git
history on 2026-11-09.

| Folder | Upstream | Commit | Commit date |
|---|---|---|---|
| `server/` | https://github.com/apache/guacamole-server | bb1ba8ed | 2026-11-03 |
| `client/` | https://github.com/apache/guacamole-client | a8daafeb3 | 2026-11-03 |

To pull in later upstream fixes, diff the upstream repo between the commit
above and the new one, and apply that diff to the folder.

`video/` derives from https://github.com/tembo/voxl at commit 3eda628
(2026-11-08), voxl-server 0.3.4.

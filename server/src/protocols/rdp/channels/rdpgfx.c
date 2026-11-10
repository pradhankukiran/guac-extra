/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

#include "channels/rdpgfx.h"
#include "plugins/channels.h"
#include "rdp.h"
#include "settings.h"

#include <freerdp/client/rdpgfx.h>
#include <freerdp/codec/h264.h>
#include <freerdp/freerdp.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/event.h>
#include <guacamole/client.h>
#include <guacamole/display-video.h>
#include <guacamole/mem.h>

#include <stdlib.h>
#include <string.h>

/**
 * Callback which associates handlers specific to Guacamole with the
 * RdpgfxClientContext instance allocated by FreeRDP to deal with received
 * RDPGFX (Graphics Pipeline) messages.
 *
 * This function is called whenever a channel connects via the PubSub event
 * system within FreeRDP, but only has any effect if the connected channel is
 * the RDPGFX channel. This specific callback is registered with the
 * PubSub system of the relevant rdpContext when guac_rdp_rdpgfx_load_plugin() is
 * called.
 *
 * @param context
 *     The rdpContext associated with the active RDP session.
 *
 * @param args
 *     Event-specific arguments, mainly the name of the channel, and a
 *     reference to the associated plugin loaded for that channel by FreeRDP.
 */
/**
 * guac-extra: FreeRDP's own handler for surface commands (decoding into the
 * GDI surface), called after the H.264 of AVC420 commands is offered for
 * passthrough. One RDP connection per process, so a static is enough.
 */
static pcRdpgfxSurfaceCommand guac_rdp_rdpgfx_surface_command_next = NULL;

/**
 * guac-extra: offers the H.264 of AVC420 surface commands for the surface
 * shown at the top-left of the screen to the display's video
 * (passthrough), then decodes as usual so guacd's copy of the screen stays
 * current.
 */
static UINT guac_rdp_rdpgfx_surface_command(RdpgfxClientContext* rdpgfx,
        const RDPGFX_SURFACE_COMMAND* cmd) {

    if (cmd->codecId == RDPGFX_CODECID_AVC420 && cmd->extra != NULL
            && rdpgfx->custom != NULL && rdpgfx->GetSurfaceData != NULL) {

        rdpGdi* gdi = (rdpGdi*) rdpgfx->custom;
        guac_client* client = ((rdp_freerdp_context*) gdi->context)->client;
        guac_rdp_client* rdp_client = (guac_rdp_client*) client->data;

        gdiGfxSurface* surface = (gdiGfxSurface*) rdpgfx->GetSurfaceData(rdpgfx,
                (UINT16) cmd->surfaceId);

        if (surface != NULL && surface->outputMapped
                && surface->outputOriginX == 0 && surface->outputOriginY == 0
                && rdp_client->display != NULL) {

            const RDPGFX_AVC420_BITMAP_STREAM* bs =
                (const RDPGFX_AVC420_BITMAP_STREAM*) cmd->extra;
            UINT32 count = bs->meta.numRegionRects;

            guac_display_video_region* regions = guac_mem_alloc(count > 0 ? count : 1,
                    sizeof(guac_display_video_region));
            for (UINT32 i = 0; i < count; i++) {
                const RECTANGLE_16* rect = &bs->meta.regionRects[i];
                regions[i].x = rect->left;
                regions[i].y = rect->top;
                regions[i].width = rect->right - rect->left;
                regions[i].height = rect->bottom - rect->top;
            }

            guac_display_video_passthrough(rdp_client->display, bs->data,
                    bs->length, regions, (int) count,
                    (int) surface->width, (int) surface->height);

            guac_mem_free(regions);

        }

    }

    return guac_rdp_rdpgfx_surface_command_next(rdpgfx, cmd);

}

static void guac_rdp_rdpgfx_channel_connected(rdpContext* context,
        ChannelConnectedEventArgs* args) {

    guac_client* client = ((rdp_freerdp_context*) context)->client;

    /* Ignore connection event if it's not for the RDPGFX channel */
    if (strcmp(args->name, RDPGFX_DVC_CHANNEL_NAME) != 0)
        return;

    /* Init GDI-backed support for the Graphics Pipeline */
    RdpgfxClientContext* rdpgfx = (RdpgfxClientContext*) args->pInterface;
    rdpGdi* gdi = context->gdi;

    if (!gdi_graphics_pipeline_init(gdi, rdpgfx))
        guac_client_log(client, GUAC_LOG_WARNING, "Rendering backend for RDPGFX "
                "channel could not be loaded. Graphics may not render at all!");
    else {

        guac_client_log(client, GUAC_LOG_DEBUG, "RDPGFX channel will be used for "
                "the RDP Graphics Pipeline Extension.");

        /* guac-extra: look at H.264 surface commands before FreeRDP decodes
         * them, to forward them to video users (video-passthrough) */
        guac_rdp_client* rdp_client = (guac_rdp_client*) client->data;
        if (rdp_client->settings->enable_video
                && rdp_client->settings->video.passthrough
                && rdpgfx->SurfaceCommand != guac_rdp_rdpgfx_surface_command) {
            guac_rdp_rdpgfx_surface_command_next = rdpgfx->SurfaceCommand;
            rdpgfx->SurfaceCommand = guac_rdp_rdpgfx_surface_command;
        }

    }

}

/**
 * Callback which handles any RDPGFX cleanup specific to Guacamole.
 *
 * This function is called whenever a channel disconnects via the PubSub event
 * system within FreeRDP, but only has any effect if the disconnected channel
 * is the RDPGFX channel. This specific callback is registered with the PubSub
 * system of the relevant rdpContext when guac_rdp_rdpgfx_load_plugin() is
 * called.
 *
 * @param context
 *     The rdpContext associated with the active RDP session.
 *
 * @param args
 *     Event-specific arguments, mainly the name of the channel, and a
 *     reference to the associated plugin loaded for that channel by FreeRDP.
 */
static void guac_rdp_rdpgfx_channel_disconnected(rdpContext* context,
        ChannelDisconnectedEventArgs* args) {

    guac_client* client = ((rdp_freerdp_context*) context)->client;

    /* Ignore disconnection event if it's not for the RDPGFX channel */
    if (strcmp(args->name, RDPGFX_DVC_CHANNEL_NAME) != 0)
        return;

    /* Un-init GDI-backed support for the Graphics Pipeline */
    RdpgfxClientContext* rdpgfx = (RdpgfxClientContext*) args->pInterface;
    rdpGdi* gdi = context->gdi;
    gdi_graphics_pipeline_uninit(gdi, rdpgfx);

    guac_client_log(client, GUAC_LOG_DEBUG, "RDPGFX channel support unloaded.");

}

void guac_rdp_rdpgfx_load_plugin(rdpContext* context) {

    /* Subscribe to and handle channel connected events */
    PubSub_SubscribeChannelConnected(context->pubSub,
        (pChannelConnectedEventHandler) guac_rdp_rdpgfx_channel_connected);

    /* Subscribe to and handle channel disconnected events */
    PubSub_SubscribeChannelDisconnected(context->pubSub,
        (pChannelDisconnectedEventHandler) guac_rdp_rdpgfx_channel_disconnected);

    /* Add "rdpgfx" channel */
    guac_freerdp_dynamic_channel_collection_add(context->settings, "rdpgfx", NULL);

}


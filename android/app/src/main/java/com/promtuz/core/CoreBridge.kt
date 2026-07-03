package com.promtuz.core

import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.withContext
import uniffi.core.ContactInfo
import uniffi.core.InvitePreview
import uniffi.core.MessageEvent
import uniffi.core.MessageRecord
import uniffi.core.computeQrMask as ffiComputeQrMask
import uniffi.core.enroll as ffiEnroll
import uniffi.core.getContacts as ffiGetContacts
import uniffi.core.getConversations as ffiGetConversations
import uniffi.core.getMessages as ffiGetMessages
import uniffi.core.makeInviteQr as ffiMakeInviteQr
import uniffi.core.pairFromQr as ffiPairFromQr
import uniffi.core.previewInvite as ffiPreviewInvite
import uniffi.core.sendMessage as ffiSendMessage
import uniffi.core.shouldLaunchApp as ffiShouldLaunchApp

/**
 * Idiomatic Kotlin facade over the uniffi-generated bindings — the single
 * seam the app talks to. Blocking DB/setup calls run on [Dispatchers.IO];
 * fallible calls throw `uniffi.core.CoreException`. Fire-and-forget calls
 * (sendMessage, pairFromQr) return once queued — their real outcome arrives
 * via [messageEvents] / the contact list, so Ok is NOT "delivered/paired".
 *
 * IPKs are always 32 bytes; callers pass them straight through as ByteArray.
 */
object CoreBridge {
    /** Cheap identity check; safe to call before [CoreInitializer.start]. */
    fun shouldLaunchApp(): Boolean = ffiShouldLaunchApp()

    suspend fun enroll(name: String) = withContext(Dispatchers.IO) { ffiEnroll(name) }

    suspend fun makeInviteQr(): ByteArray = withContext(Dispatchers.IO) { ffiMakeInviteQr() }

    suspend fun pairFromQr(qrBytes: ByteArray) = withContext(Dispatchers.IO) { ffiPairFromQr(qrBytes) }

    /** Decode-only preview of an invite (QR or link) for the confirm sheet; no pairing. */
    suspend fun previewInvite(bytes: ByteArray): InvitePreview =
        withContext(Dispatchers.IO) { ffiPreviewInvite(bytes) }

    suspend fun contacts(): List<ContactInfo> = withContext(Dispatchers.IO) { ffiGetContacts() }

    suspend fun conversations(): List<MessageRecord> = withContext(Dispatchers.IO) { ffiGetConversations() }

    suspend fun messages(peerIpk: ByteArray, limit: Int, beforeId: String = ""): List<MessageRecord> =
        withContext(Dispatchers.IO) { ffiGetMessages(peerIpk, limit.toUInt(), beforeId) }

    suspend fun sendMessage(toIpk: ByteArray, content: String) =
        withContext(Dispatchers.IO) { ffiSendMessage(toIpk, content) }

    /** Pure render helper; safe on any thread (used from the QR View). */
    fun computeQrMask(grid: ByteArray, size: Int): ByteArray = ffiComputeQrMask(grid, size.toUInt())

    /** Latest connection state, mapped to the app enum (carries @StringRes). */
    val connection: StateFlow<ConnectionState> get() = CoreEventBus.connection

    /** Inbound/outbound message deltas (Received / Sent / Failed). */
    val messageEvents: SharedFlow<MessageEvent> get() = CoreEventBus.messages
}

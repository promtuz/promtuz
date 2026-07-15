package com.promtuz.core

import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.withContext
import uniffi.core.ContactDiag
import uniffi.core.ContactInfo
import uniffi.core.InvitePreview
import uniffi.core.MessageRecord
import uniffi.core.ReactionRecord
import uniffi.core.RelayStat
import uniffi.core.UnreadCount
import uniffi.core.computeQrMask as ffiComputeQrMask
import uniffi.core.connectRelay as ffiConnectRelay
import uniffi.core.enroll as ffiEnroll
import uniffi.core.forgetContact as ffiForgetContact
import uniffi.core.forgetRelay as ffiForgetRelay
import uniffi.core.getContacts as ffiGetContacts
import uniffi.core.getConversations as ffiGetConversations
import uniffi.core.getMessages as ffiGetMessages
import uniffi.core.getRelays as ffiGetRelays
import uniffi.core.listContactsDiag as ffiListContactsDiag
import uniffi.core.makeInviteQr as ffiMakeInviteQr
import uniffi.core.pairFromQr as ffiPairFromQr
import uniffi.core.previewInvite as ffiPreviewInvite
import uniffi.core.resetRelayCircuit as ffiResetRelayCircuit
import uniffi.core.sendMessage as ffiSendMessage
import uniffi.core.shouldLaunchApp as ffiShouldLaunchApp
import uniffi.core.deleteMessage as ffiDeleteMessage
import uniffi.core.editMessage as ffiEditMessage
import uniffi.core.markRead as ffiMarkRead
import uniffi.core.markConversationRead as ffiMarkConversationRead
import uniffi.core.unreadCounts as ffiUnreadCounts
import uniffi.core.reactMessage as ffiReactMessage
import uniffi.core.reactionsFor as ffiReactionsFor
import uniffi.core.setActivity as ffiSetActivity
import uniffi.core.subscribePresence as ffiSubscribePresence
import uniffi.core.onForeground as ffiOnForeground
import uniffi.core.onTaskRemoved as ffiOnTaskRemoved
import uniffi.core.registerPushToken as ffiRegisterPushToken
import uniffi.core.registerPush as ffiRegisterPush
import uniffi.core.kpPublishReady as ffiKpPublishReady
import uniffi.core.setPresence as ffiSetPresence
import uniffi.core.adoptEscrowedSecret as ffiAdoptEscrowedSecret
import uniffi.core.backupExport as ffiBackupExport
import uniffi.core.backupImport as ffiBackupImport
import uniffi.core.escrowSecret as ffiEscrowSecret
import uniffi.core.exportRecoveryPhrase as ffiExportRecoveryPhrase
import uniffi.core.restoreFromPhrase as ffiRestoreFromPhrase
import com.promtuz.core.adapter.ActivitySignal
import com.promtuz.core.adapter.PresenceSignal

/**
 * Idiomatic Kotlin facade over the uniffi-generated bindings — the single
 * seam the app talks to. Blocking DB/setup calls run on [Dispatchers.IO];
 * fallible calls throw `uniffi.core.CoreException`. Fire-and-forget calls
 * (sendMessage, pairFromQr) return once queued — their real outcome surfaces
 * by observing the DB (the [dbChanged] doorbell), so Ok is NOT "delivered/paired".
 *
 * IPKs are always 32 bytes; callers pass them straight through as ByteArray.
 */
object CoreBridge {
    /** App returned to foreground — wake the relay loop for an instant reconnect. */
    fun onForeground() = ffiOnForeground()

    /** App task was removed from recents — best-effort close so relay marks us offline. */
    fun onTaskRemoved() = ffiOnTaskRemoved()

    /** Hand libcore the FCM push token; it registers `P → token` with a gateway. Fire-and-forget. */
    fun registerPushToken(token: ByteArray) = ffiRegisterPushToken(token)

    /** Re-assert our push pseudonym with the home relay. Auto-runs on connect; rarely needed manually. */
    fun registerPush() = ffiRegisterPush()

    /** Assert our activity mode to contacts: idle on background, active on foreground. */
    fun setPresence(idle: Boolean) = ffiSetPresence(idle)

    /** Cheap identity check; safe to call before [CoreInitializer.start]. */
    fun shouldLaunchApp(): Boolean = ffiShouldLaunchApp()

    /** Are we discoverable (KeyPackage quorum-published)? Gate the share QR on this. */
    fun kpPublishReady(): Boolean = ffiKpPublishReady()

    suspend fun enroll(name: String) = withContext(Dispatchers.IO) { ffiEnroll(name) }

    // — Identity recovery (IDENTITY_RECOVERY.md). The two exports below are
    //   the identity in raw/word form: EVERY call site must sit behind a
    //   device-auth gate (see RecoveryStore / RecoveryPhraseScreen).

    /** The identity as a 24-word BIP39 phrase. AUTH-GATE MANDATORY. */
    suspend fun exportRecoveryPhrase(): List<String> =
        withContext(Dispatchers.IO) { ffiExportRecoveryPhrase() }

    /** Raw isk for Block Store escrow. AUTH-GATE MANDATORY. */
    suspend fun escrowSecret(): ByteArray = withContext(Dispatchers.IO) { ffiEscrowSecret() }

    /** Restore identity from a typed phrase; throws on bad checksum or if an identity exists. */
    suspend fun restoreFromPhrase(words: List<String>, name: String) =
        withContext(Dispatchers.IO) { ffiRestoreFromPhrase(words, name) }

    /** Restore identity from escrowed bytes (Block Store hit on fresh install). */
    suspend fun adoptEscrowedSecret(isk: ByteArray, name: String) =
        withContext(Dispatchers.IO) { ffiAdoptEscrowedSecret(isk, name) }

    /** Snapshot history+contacts+name into one encrypted blob (ciphertext-only to cloud). */
    suspend fun backupExport(): ByteArray = withContext(Dispatchers.IO) { ffiBackupExport() }

    /** Restore a backup blob (after identity restore); idempotent. */
    suspend fun backupImport(blob: ByteArray) = withContext(Dispatchers.IO) { ffiBackupImport(blob) }

    suspend fun makeInviteQr(): ByteArray = withContext(Dispatchers.IO) { ffiMakeInviteQr() }

    suspend fun pairFromQr(qrBytes: ByteArray) = withContext(Dispatchers.IO) { ffiPairFromQr(qrBytes) }

    /** Decode-only preview of an invite (QR or link) for the confirm sheet; no pairing. */
    suspend fun previewInvite(bytes: ByteArray): InvitePreview =
        withContext(Dispatchers.IO) { ffiPreviewInvite(bytes) }

    suspend fun contacts(): List<ContactInfo> = withContext(Dispatchers.IO) { ffiGetContacts() }

    /** Contacts + per-contact diagnostics (paired, MLS epoch, msg count/status, pending ops). */
    suspend fun contactsDiag(): List<ContactDiag> = withContext(Dispatchers.IO) { ffiListContactsDiag() }

    /**
     * Delete a contact and ALL its local state — MLS group, message history,
     * epoch buffer, outbox rows — so re-scanning their QR is a clean first-time
     * add. Irreversible; the peer isn't notified.
     */
    suspend fun forgetContact(ipk: ByteArray) = withContext(Dispatchers.IO) { ffiForgetContact(ipk) }

    suspend fun conversations(): List<MessageRecord> = withContext(Dispatchers.IO) { ffiGetConversations() }

    suspend fun messages(peerIpk: ByteArray, limit: Int, beforeId: String = ""): List<MessageRecord> =
        withContext(Dispatchers.IO) { ffiGetMessages(peerIpk, limit.toUInt(), beforeId) }

    suspend fun sendMessage(toIpk: ByteArray, content: String, replyTo: ByteArray? = null) =
        withContext(Dispatchers.IO) { ffiSendMessage(toIpk, content, replyTo) }

    suspend fun editMessage(peer: ByteArray, dispatchId: ByteArray, content: String) =
        withContext(Dispatchers.IO) { ffiEditMessage(peer, dispatchId, content) }

    /** Delete for everyone (tombstones both sides) or just locally. */
    suspend fun deleteMessage(peer: ByteArray, dispatchId: ByteArray, forEveryone: Boolean) =
        withContext(Dispatchers.IO) { ffiDeleteMessage(peer, dispatchId, forEveryone) }

    /** Add/remove our own `emoji` reaction on a message. */
    suspend fun react(peer: ByteArray, dispatchId: ByteArray, emoji: String, add: Boolean) =
        withContext(Dispatchers.IO) { ffiReactMessage(peer, dispatchId, emoji, add) }

    suspend fun reactions(peer: ByteArray): List<ReactionRecord> =
        withContext(Dispatchers.IO) { ffiReactionsFor(peer) }

    /** High-water-mark read receipt: mark everything from `peer` up to this dispatch id as read. */
    suspend fun markRead(peer: ByteArray, uptoDispatchId: ByteArray) =
        withContext(Dispatchers.IO) { ffiMarkRead(peer, uptoDispatchId) }

    /** Mark the whole conversation with `peer` read (home-list action). */
    suspend fun markConversationRead(peer: ByteArray) =
        withContext(Dispatchers.IO) { ffiMarkConversationRead(peer) }

    /** Per-peer unread incoming counts (only peers with unread > 0) for home badges. */
    suspend fun unreadCounts(): List<UnreadCount> =
        withContext(Dispatchers.IO) { ffiUnreadCounts() }

    /** Ephemeral typing/recording signal (OR of Activity bits; 0 = idle). Fire-and-forget. */
    suspend fun setActivity(peer: ByteArray, activityBits: Int) =
        withContext(Dispatchers.IO) { ffiSetActivity(peer, activityBits.toUShort()) }

    /** (Re)subscribe presence interest to these contacts. */
    suspend fun subscribePresence(contacts: List<ByteArray>) =
        withContext(Dispatchers.IO) { ffiSubscribePresence(contacts) }

    /** Pure render helper; safe on any thread (used from the QR View). */
    fun computeQrMask(grid: ByteArray, size: Int): ByteArray = ffiComputeQrMask(grid, size.toUInt())

    /** All stored relays with health + latency history (diagnostics page). */
    suspend fun relays(): List<RelayStat> = withContext(Dispatchers.IO) { ffiGetRelays() }

    /** Un-trip a relay's circuit breaker so it's reconsidered immediately. */
    suspend fun resetRelayCircuit(id: String) = withContext(Dispatchers.IO) { ffiResetRelayCircuit(id) }

    /** Delete a relay locally; the resolver re-adds it on the next fetch. */
    suspend fun forgetRelay(id: String) = withContext(Dispatchers.IO) { ffiForgetRelay(id) }

    /** Connect (or reconnect) to a specific relay by id. */
    suspend fun connectRelay(id: String) = withContext(Dispatchers.IO) { ffiConnectRelay(id) }

    /** Latest connection state, mapped to the app enum (carries @StringRes). */
    val connection: StateFlow<ConnectionState> get() = CoreEventBus.connection

    /** The reactive doorbell: "these tables changed, re-read." Drives [observeQuery]. */
    val dbChanged: SharedFlow<Set<String>> get() = CoreEventBus.dbChanged

    /** Ephemeral peer typing/recording signals (not stored; UI times them out). */
    val activity: SharedFlow<ActivitySignal> get() = CoreEventBus.activity

    /** Ephemeral peer presence changes (online / last-seen). */
    val presence: SharedFlow<PresenceSignal> get() = CoreEventBus.presence

    /** Last-known presence per peer (hex IPK) — what a freshly opened chat reads first. */
    val presenceByPeer get() = CoreEventBus.presenceByPeer
}

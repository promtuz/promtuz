package com.promtuz.core

import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.withContext
import uniffi.core.BackupMergeReport
import uniffi.core.ContactDiag
import uniffi.core.ContactInfo
import uniffi.core.InvitePreview
import uniffi.core.MediaRecord
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
import uniffi.core.verifyUpdateManifest as ffiVerifyUpdateManifest
import uniffi.core.reactMessage as ffiReactMessage
import uniffi.core.reactionsFor as ffiReactionsFor
import uniffi.core.setActivity as ffiSetActivity
import uniffi.core.subscribePresence as ffiSubscribePresence
import uniffi.core.onForeground as ffiOnForeground
import uniffi.core.onTaskRemoved as ffiOnTaskRemoved
import uniffi.core.syncMessages as ffiSyncMessages
import uniffi.core.registerPushToken as ffiRegisterPushToken
import uniffi.core.registerPush as ffiRegisterPush
import uniffi.core.kpPublishReady as ffiKpPublishReady
import uniffi.core.setPresence as ffiSetPresence
import uniffi.core.adoptEscrowedSecret as ffiAdoptEscrowedSecret
import uniffi.core.backupExport as ffiBackupExport
import uniffi.core.backupImport as ffiBackupImport
import uniffi.core.backupImportMerge as ffiBackupImportMerge
import uniffi.core.escrowSecret as ffiEscrowSecret
import uniffi.core.exportRecoveryPhrase as ffiExportRecoveryPhrase
import uniffi.core.restoreFromPhrase as ffiRestoreFromPhrase
import uniffi.core.StagedRecord
import uniffi.core.stageImage as ffiStageImage
import uniffi.core.stageAttachment as ffiStageAttachment
import uniffi.core.discardStaged as ffiDiscardStaged
import uniffi.core.clearStaged as ffiClearStaged
import uniffi.core.stagedItems as ffiStagedItems
import uniffi.core.sendStaged as ffiSendStaged
import uniffi.core.reviseWithStaged as ffiReviseWithStaged
import uniffi.core.downloadAttachment as ffiDownloadAttachment
import uniffi.core.getMedia as ffiGetMedia
import uniffi.core.sendVoice as ffiSendVoice
import uniffi.core.StickerPackPreview
import uniffi.core.StickerPackRecord
import uniffi.core.StickerRecord
import uniffi.core.StickerSource
import uniffi.core.stickerPacks as ffiStickerPacks
import uniffi.core.recentStickers as ffiRecentStickers
import uniffi.core.stickerImage as ffiStickerImage
import uniffi.core.stickerPackPreview as ffiStickerPackPreview
import uniffi.core.installStickerPack as ffiInstallStickerPack
import uniffi.core.removeStickerPack as ffiRemoveStickerPack
import uniffi.core.createStickerPack as ffiCreateStickerPack
import uniffi.core.addToStickerPack as ffiAddToStickerPack
import uniffi.core.sendSticker as ffiSendSticker
import uniffi.core.refreshStickerPacks as ffiRefreshStickerPacks
import uniffi.core.searchMessages as ffiSearchMessages
import uniffi.core.SearchHit
import uniffi.core.ConversationRecord
import uniffi.core.MemberRecord
import uniffi.core.listConversations as ffiListConversations
import uniffi.core.getConversation as ffiGetConversation
import uniffi.core.conversationWith as ffiConversationWith
import uniffi.core.conversationMembers as ffiConversationMembers
import uniffi.core.seenByCount as ffiSeenByCount
import uniffi.core.setConversationTitle as ffiSetConversationTitle
import uniffi.core.createGroup as ffiCreateGroup
import uniffi.core.addGroupMember as ffiAddGroupMember
import uniffi.core.removeGroupMember as ffiRemoveGroupMember
import uniffi.core.leaveGroup as ffiLeaveGroup
import uniffi.core.deleteConversation as ffiDeleteConversation
import uniffi.core.clearConversationHistory as ffiClearConversationHistory
import uniffi.core.setConversationPinned as ffiSetConversationPinned
import uniffi.core.setConversationMuted as ffiSetConversationMuted
import uniffi.core.pendingNotificationIds as ffiPendingNotificationIds
import uniffi.core.markNotified as ffiMarkNotified
import uniffi.core.getPref as ffiGetPref
import uniffi.core.setPref as ffiSetPref
import uniffi.core.recentIncoming as ffiRecentIncoming
import uniffi.core.inviteLink as ffiInviteLink
import uniffi.core.inviteFromLink as ffiInviteFromLink
import uniffi.core.timeBucket as ffiTimeBucket
import uniffi.core.validateUpdateManifest as ffiValidateUpdateManifest
import uniffi.core.updateIsInstallable as ffiUpdateIsInstallable
import uniffi.core.updateIsRequired as ffiUpdateIsRequired
import uniffi.core.profileName as ffiProfileName
import uniffi.core.profilePicture as ffiProfilePicture
import uniffi.core.setProfilePicture as ffiSetProfilePicture
import uniffi.core.clearProfilePicture as ffiClearProfilePicture
import uniffi.core.avatarOf as ffiAvatarOf
import uniffi.core.avatarGeneration as ffiAvatarGeneration
import uniffi.core.TimeBucket
import uniffi.core.UpdateManifest
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

    /** Returns after a gateway has persisted the FCM registration. */
    suspend fun registerPushToken(token: ByteArray) = ffiRegisterPushToken(token)

    suspend fun syncMessages() = ffiSyncMessages()

    /** Re-assert our push pseudonym with the home relay. Auto-runs on connect; rarely needed manually. */
    fun registerPush() = ffiRegisterPush()

    /** Assert our activity mode to contacts: idle on background, active on foreground. */
    fun setPresence(idle: Boolean) = ffiSetPresence(idle)

    /** Cheap identity check; safe to call before [CoreInitializer.start]. */
    fun shouldLaunchApp(): Boolean = ffiShouldLaunchApp()

    /** Verify signed update metadata before its bytes are decoded as JSON. */
    fun verifyUpdateManifest(manifest: ByteArray, signature: ByteArray): Boolean =
        ffiVerifyUpdateManifest(manifest, signature)

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

    /**
     * Additive restore: inserts only rows we don't already have, never
     * replaces or renames. Safe against a live DB — unlike [backupImport],
     * whose replace semantics assume the fresh install of a reinstall.
     */
    suspend fun backupImportMerge(blob: ByteArray): BackupMergeReport =
        withContext(Dispatchers.IO) { ffiBackupImportMerge(blob) }

    suspend fun makeInviteQr(): ByteArray = withContext(Dispatchers.IO) { ffiMakeInviteQr() }

    suspend fun pairFromQr(qrBytes: ByteArray) = withContext(Dispatchers.IO) { ffiPairFromQr(qrBytes) }

    /** Decode-only preview of an invite (QR or link) for the confirm sheet; no pairing. */
    suspend fun previewInvite(bytes: ByteArray): InvitePreview =
        withContext(Dispatchers.IO) { ffiPreviewInvite(bytes) }

    suspend fun contacts(): List<ContactInfo> = withContext(Dispatchers.IO) { ffiGetContacts() }

    /** Our display name, as enrolled or last restored. */
    suspend fun profileName(): String = withContext(Dispatchers.IO) { ffiProfileName() }

    suspend fun contactCard(ipk: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.contactCard(ipk) }
    suspend fun previewContactCard(bytes: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.previewContactCard(bytes) }
    suspend fun requestContact(bytes: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.requestContact(bytes) }
    suspend fun contactRequests() = withContext(Dispatchers.IO) { uniffi.core.contactRequests() }
    suspend fun acceptContactRequest(ipk: ByteArray) = uniffi.core.acceptContactRequest(ipk)
    suspend fun dismissContactRequest(ipk: ByteArray, outgoing: Boolean) = withContext(Dispatchers.IO) { uniffi.core.dismissContactRequest(ipk, outgoing) }

    suspend fun profileBio(): String = withContext(Dispatchers.IO) { uniffi.core.profileBio() }
    suspend fun setProfileDetails(name: String, bio: String) = withContext(Dispatchers.IO) { uniffi.core.setProfileDetails(name, bio) }
    suspend fun personProfile(ipk: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.personProfile(ipk) }
    suspend fun setContactNickname(ipk: ByteArray, name: String) = withContext(Dispatchers.IO) { uniffi.core.setContactNickname(ipk, name) }
    suspend fun groupPicture(conv: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.groupPicture(conv) }
    suspend fun setGroupPicture(conv: ByteArray, rgba: ByteArray?, width: Int = 0, height: Int = 0) =
        uniffi.core.setGroupPicture(conv, rgba, width.toUInt(), height.toUInt())

    /** Our profile picture as AVIF bytes, or null when we have none. */
    suspend fun profilePicture(): ByteArray? = withContext(Dispatchers.IO) { ffiProfilePicture() }

    /**
     * Set the profile picture from a decoded bitmap's RGBA. Core crops, scales and
     * encodes it, stores it and tells every chat; returns once it is stored, the
     * sends landing on their own like any control message.
     */
    suspend fun setProfilePicture(rgba: ByteArray, width: Int, height: Int) =
        withContext(Dispatchers.IO) { ffiSetProfilePicture(rgba, width.toUInt(), height.toUInt()) }

    /** Remove the profile picture and tell every chat to stop showing it. */
    suspend fun clearProfilePicture() = withContext(Dispatchers.IO) { ffiClearProfilePicture() }

    /** The picture to draw for [ipk] (ours for ourselves) as AVIF bytes; null draws initials. */
    suspend fun avatarOf(ipk: ByteArray): ByteArray? = withContext(Dispatchers.IO) { ffiAvatarOf(ipk) }

    /** Moves whenever any picture changes; cheaper to compare than to re-decode on every doorbell. */
    suspend fun avatarGeneration(): ULong = withContext(Dispatchers.IO) { ffiAvatarGeneration() }

    /** Contacts + per-contact diagnostics (paired, MLS epoch, msg count/status, pending ops). */
    suspend fun contactsDiag(): List<ContactDiag> = withContext(Dispatchers.IO) { ffiListContactsDiag() }

    /**
     * Delete a contact and ALL its local state — MLS group, message history,
     * epoch buffer, outbox rows — so re-scanning their QR is a clean first-time
     * add. Irreversible; the peer isn't notified.
     */
    suspend fun forgetContact(ipk: ByteArray) = withContext(Dispatchers.IO) { ffiForgetContact(ipk) }

    /** Latest message per conversation — the home list's preview line. */
    suspend fun storagePreview(conversation: ByteArray, dispatch: ByteArray) =
        withContext(Dispatchers.IO) { uniffi.core.storagePreview(conversation, dispatch) }
    suspend fun storageMedia() = withContext(Dispatchers.IO) { uniffi.core.storageMedia() }
    suspend fun removeStoredMedia(targets: List<uniffi.core.StorageTarget>) =
        withContext(Dispatchers.IO) { uniffi.core.removeStoredMedia(targets) }

    suspend fun conversations(): List<MessageRecord> = withContext(Dispatchers.IO) { ffiGetConversations() }

    /** Every conversation with its roster and title — the home list's rows. */
    suspend fun listConversations(): List<ConversationRecord> =
        withContext(Dispatchers.IO) { ffiListConversations() }

    /** One conversation by id, or null once it's gone. */
    suspend fun conversation(id: ByteArray): ConversationRecord? =
        withContext(Dispatchers.IO) { ffiGetConversation(id) }

    /**
     * The direct conversation with a contact, created on first open. How a
     * person in the contacts list becomes a chat.
     */
    suspend fun conversationWith(peerIpk: ByteArray): ByteArray =
        withContext(Dispatchers.IO) { ffiConversationWith(peerIpk) }

    /**
     * Drop a conversation, its history and its keys from this device. Nobody is
     * told and no membership changes, but a group's messages stop arriving and
     * the chat does not come back — leaving is the separate act that tells the
     * others. Refused for a group you founded while others remain, unless
     * [force] — the escape hatch for a group whose own state is what broke.
     */
    suspend fun deleteConversation(conversationId: ByteArray, force: Boolean = false) =
        withContext(Dispatchers.IO) { ffiDeleteConversation(conversationId, force) }

    /** Empty a chat of its messages but keep the chat. Local only, always allowed. */
    suspend fun clearConversationHistory(conversationId: ByteArray) =
        withContext(Dispatchers.IO) { ffiClearConversationHistory(conversationId) }

    suspend fun setConversationPinned(id: ByteArray, pinned: Boolean) =
        withContext(Dispatchers.IO) { ffiSetConversationPinned(id, pinned) }

    suspend fun setConversationMuted(id: ByteArray, muted: Boolean) =
        withContext(Dispatchers.IO) { ffiSetConversationMuted(id, muted) }

    /** An app setting, or null if never set. Stored in core so it survives a reinstall. */
    suspend fun pref(key: String): String? = withContext(Dispatchers.IO) { ffiGetPref(key) }

    suspend fun setPref(key: String, value: String) =
        withContext(Dispatchers.IO) { ffiSetPref(key, value) }

    /** The lines a notification summarises: newest incoming, undeleted, oldest-first. */
    suspend fun recentIncoming(id: ByteArray, limit: Int): List<MessageRecord> =
        withContext(Dispatchers.IO) { ffiRecentIncoming(id, limit.toUInt()) }

    /** The shareable pair link, and its inverse. A URL contract, so core owns both ends. */
    fun inviteLink(invite: ByteArray): String = ffiInviteLink(invite)
    fun inviteFromLink(url: String): ByteArray? = ffiInviteFromLink(url)

    /** Which bucket a timestamp falls into; the platform turns it into words. */
    fun timeBucket(tsMs: Long, nowMs: Long, utcOffsetSecs: Int): TimeBucket =
        ffiTimeBucket(tsMs.toULong(), nowMs.toULong(), utcOffsetSecs)

    fun validateUpdateManifest(m: UpdateManifest) = ffiValidateUpdateManifest(m)
    fun updateIsInstallable(offered: Int, installed: Long, switchingChannel: Boolean) =
        ffiUpdateIsInstallable(offered.toUInt(), installed.toULong(), switchingChannel)
    /** A major or minor step the installed build cannot skip. */
    fun updateIsRequired(installed: String, offered: String) = ffiUpdateIsRequired(installed, offered)

    /** Full roster, departed members included so old messages still name someone. */
    suspend fun members(conversationId: ByteArray): List<MemberRecord> =
        withContext(Dispatchers.IO) { ffiConversationMembers(conversationId) }

    /** How many members have read up to this message — the "seen by N" figure. */
    suspend fun seenBy(conversationId: ByteArray, dispatchId: ByteArray): Int =
        withContext(Dispatchers.IO) { ffiSeenByCount(conversationId, dispatchId).toInt() }

    /** Rename a group locally. v1 does not broadcast the change. */
    suspend fun setConversationTitle(conversationId: ByteArray, title: String) =
        withContext(Dispatchers.IO) { ffiSetConversationTitle(conversationId, title) }

    // — Group membership. Each needs a live relay (a KeyPackage fetch and a
    //   Welcome), so these report failure rather than queueing like a message.

    /** Create a group with us as admin. Returns the new conversation id. */
    suspend fun createGroup(title: String, members: List<ByteArray>): ByteArray =
        withContext(Dispatchers.IO) { ffiCreateGroup(title, members) }

    /** Add someone. Admin-only; they see no history from before they joined. */
    suspend fun addGroupMember(conversationId: ByteArray, memberIpk: ByteArray) =
        withContext(Dispatchers.IO) { ffiAddGroupMember(conversationId, memberIpk) }

    /** Remove someone, rotating keys so their device can't read what follows. */
    suspend fun removeGroupMember(conversationId: ByteArray, memberIpk: ByteArray) =
        withContext(Dispatchers.IO) { ffiRemoveGroupMember(conversationId, memberIpk) }

    /** Leave. The chat and its history stay; it just can't send any more. */
    suspend fun leaveGroup(conversationId: ByteArray) =
        withContext(Dispatchers.IO) { ffiLeaveGroup(conversationId) }

    suspend fun messages(conversationId: ByteArray, limit: Int, beforeId: String = ""): List<MessageRecord> =
        withContext(Dispatchers.IO) { ffiGetMessages(conversationId, limit.toUInt(), beforeId) }

    suspend fun sendMessage(conversationId: ByteArray, content: String, replyTo: ByteArray? = null) =
        withContext(Dispatchers.IO) { ffiSendMessage(conversationId, content, replyTo) }

    // — Media (images + attachments). Unsigned FFI dimensions/sizes are taken as Int here
    //   and widened once at the boundary, matching messages()/setActivity().

    // — Composer staging. Picking media puts it in a buffer and starts the
    //   expensive pass (AVIF / manifest hash) immediately; the send comes later,
    //   so the encode overlaps with the caption being typed. State changes ring
    //   the doorbell under "staging" — read back through [stagedItems].

    /** Buffer a picked photo; the AVIF pass runs off-thread. Returns its id. */
    suspend fun stageImage(rgba: ByteArray, width: Int, height: Int): ULong =
        withContext(Dispatchers.IO) { ffiStageImage(rgba, width.toUInt(), height.toUInt()) }

    /** Buffer a picked file; the blur lands before this returns, the hash after. */
    suspend fun stageAttachment(
        sourcePath: String, name: String, mime: String,
        thumbRgba: ByteArray?, thumbW: Int, thumbH: Int,
    ): ULong = withContext(Dispatchers.IO) {
        ffiStageAttachment(sourcePath, name, mime, thumbRgba, thumbW.toUInt(), thumbH.toUInt())
    }

    /** Drop one buffered item. Safe mid-encode — the running pass discards its result. */
    suspend fun discardStaged(id: ULong) = withContext(Dispatchers.IO) { ffiDiscardStaged(id) }

    suspend fun commitShared(conversationId: ByteArray, ids: List<ULong>, caption: String) =
        uniffi.core.commitShared(conversationId, ids, caption)

    suspend fun clearStaged() = withContext(Dispatchers.IO) { ffiClearStaged() }

    suspend fun stagedItems(): List<StagedRecord> = withContext(Dispatchers.IO) { ffiStagedItems() }

    /** Send the buffer as one album; caption rides the first, `replyTo` rides all. */
    suspend fun sendStaged(
        conversationId: ByteArray, ids: List<ULong>, caption: String, replyTo: ByteArray? = null,
    ) = withContext(Dispatchers.IO) { ffiSendStaged(conversationId, ids, caption, replyTo) }

    /** Replace a message's body with a buffered item — the media half of an edit. */
    suspend fun reviseWithStaged(
        conversationId: ByteArray, dispatchId: ByteArray, stagedId: ULong, caption: String,
    ) = withContext(Dispatchers.IO) {
        ffiReviseWithStaged(conversationId, dispatchId, stagedId, caption)
    }

    /** Start (or resume) the device-to-device pull of an attachment's bytes. */
    suspend fun downloadAttachment(fileId: ByteArray) =
        withContext(Dispatchers.IO) { ffiDownloadAttachment(fileId) }

    /** Messages in a chat containing [query], newest first. */
    suspend fun searchMessages(conversationId: ByteArray, query: String, limit: Int = 200): List<SearchHit> =
        withContext(Dispatchers.IO) { ffiSearchMessages(conversationId, query, limit.toUInt()) }

    /** Send a recorded voice note inline. Fire-and-forget like the other sends. */
    suspend fun sendVoice(
        conversationId: ByteArray, data: ByteArray, mime: String, durationMs: Int, waveform: ByteArray,
        replyTo: ByteArray? = null,
    ) = withContext(Dispatchers.IO) {
        ffiSendVoice(conversationId, data, mime, durationMs.toUInt(), waveform, replyTo)
    }

    /** Media rows for a conversation (inline blob/thumb + transfer progress in chunks). */
    suspend fun sharedMedia(conversationId: ByteArray) = withContext(Dispatchers.IO) { uniffi.core.sharedMedia(conversationId) }

    suspend fun getMedia(conversationId: ByteArray): List<MediaRecord> =
        withContext(Dispatchers.IO) { ffiGetMedia(conversationId) }

    suspend fun getMessageMedia(conversationId: ByteArray, dispatchId: ByteArray): MediaRecord? =
        withContext(Dispatchers.IO) { uniffi.core.getMessageMedia(conversationId, dispatchId) }

    suspend fun messageAtTime(conversationId: ByteArray, timestamp: Long): uniffi.core.MessagePosition? =
        withContext(Dispatchers.IO) { uniffi.core.messageAtTime(conversationId, timestamp.coerceAtLeast(0).toULong()) }

    suspend fun stickerCacheBytes(): Long = withContext(Dispatchers.IO) { uniffi.core.stickerCacheBytes().toLong() }

    suspend fun clearStickerCache() = uniffi.core.clearStickerCache()

    // Core backs up pack references and caches downloaded images separately.

    /** Installed packs in picker order. */
    suspend fun stickerPacks(): List<StickerPackRecord> = withContext(Dispatchers.IO) { ffiStickerPacks() }

    /** Most recently sent first, from kept packs. */
    suspend fun recentStickers(limit: Int): List<StickerRecord> =
        withContext(Dispatchers.IO) { ffiRecentStickers(limit.toUInt()) }

    /** The sticker's AVIF bytes, fetched on first use. */
    suspend fun stickerImage(sticker: StickerRecord): ByteArray = ffiStickerImage(sticker)

    /** The pack behind a sticker, verified and read; nothing is kept. */
    suspend fun stickerPackPreview(sticker: StickerRecord): StickerPackPreview = ffiStickerPackPreview(sticker)

    suspend fun installStickerPack(sticker: StickerRecord) = ffiInstallStickerPack(sticker)

    suspend fun removeStickerPack(pack: ByteArray) = withContext(Dispatchers.IO) { ffiRemoveStickerPack(pack) }

    /** Publish a new pack. Returns its id once the store holds every object. */
    suspend fun createStickerPack(name: String, images: List<StickerSource>): ByteArray =
        ffiCreateStickerPack(name, images)

    suspend fun addToStickerPack(pack: ByteArray, images: List<StickerSource>) = ffiAddToStickerPack(pack, images)

    /** Save the outgoing sticker and schedule delivery. */
    suspend fun sendSticker(conversationId: ByteArray, sticker: StickerRecord, replyTo: ByteArray? = null) =
        withContext(Dispatchers.IO) { ffiSendSticker(conversationId, sticker, replyTo) }

    /** Look for appends to kept packs; core rate-limits per pack. */
    fun refreshStickerPacks() = ffiRefreshStickerPacks()

    suspend fun editMessage(conversationId: ByteArray, dispatchId: ByteArray, content: String) =
        withContext(Dispatchers.IO) { ffiEditMessage(conversationId, dispatchId, content) }

    /** Delete for everyone (tombstones both sides) or just locally. */
    suspend fun deleteMessage(conversationId: ByteArray, dispatchId: ByteArray, forEveryone: Boolean) =
        withContext(Dispatchers.IO) { ffiDeleteMessage(conversationId, dispatchId, forEveryone) }

    /** Add/remove our own `emoji` reaction on a message. */
    suspend fun react(conversationId: ByteArray, dispatchId: ByteArray, emoji: String, add: Boolean) =
        withContext(Dispatchers.IO) { ffiReactMessage(conversationId, dispatchId, emoji, add) }

    suspend fun reactions(conversationId: ByteArray): List<ReactionRecord> =
        withContext(Dispatchers.IO) { ffiReactionsFor(conversationId) }

    /** High-water-mark read receipt: mark everything up to this dispatch id as read. */
    suspend fun markRead(conversationId: ByteArray, uptoDispatchId: ByteArray) =
        withContext(Dispatchers.IO) { ffiMarkRead(conversationId, uptoDispatchId) }

    /** Mark a whole conversation read (home-list action). */
    suspend fun markConversationRead(conversationId: ByteArray) =
        withContext(Dispatchers.IO) { ffiMarkConversationRead(conversationId) }

    /** Per-conversation unread incoming counts (only those with unread > 0) for home badges. */
    suspend fun unreadCounts(): List<UnreadCount> =
        withContext(Dispatchers.IO) { ffiUnreadCounts() }

    suspend fun pendingNotificationIds(conversationId: ByteArray): List<String> =
        withContext(Dispatchers.IO) { ffiPendingNotificationIds(conversationId) }

    suspend fun markNotified(ids: List<String>) =
        withContext(Dispatchers.IO) { ffiMarkNotified(ids) }

    /** Ephemeral typing/recording signal (OR of Activity bits; 0 = idle). Fire-and-forget. */
    suspend fun setActivity(conversationId: ByteArray, activityBits: Int) =
        withContext(Dispatchers.IO) { ffiSetActivity(conversationId, activityBits.toUShort()) }

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

    // — Calls (CALLS.md). Control is synchronous and returns once queued; the
    //   call's progress arrives on CoreEvents.onCall. Audio crosses on its own
    //   real-time threads, so these two stay off the IO dispatcher.

    /** Ring a paired contact, as video when [video]. Returns the call id. */
    fun callStart(peer: ByteArray, video: Boolean = false): ByteArray = uniffi.core.callStart(peer, video)

    /** Turn our camera on or off in a video call. */
    fun callSetCamera(on: Boolean) = uniffi.core.callSetCamera(on)

    /** One encoded H.264 access unit (Annex-B) from the video encoder. */
    fun callPushVideo(frame: ByteArray, keyframe: Boolean) = uniffi.core.callPushVideo(frame, keyframe)

    /** Pick up the ringing call. */
    fun callAccept() = uniffi.core.callAccept()

    /** Refuse the ringing call. */
    fun callReject() = uniffi.core.callReject()

    /** Hang up, cancel, or refuse — whatever the current call is doing. */
    fun callHangup() = uniffi.core.callHangup()

    /** Mute or unmute our microphone. */
    fun callSetMuted(muted: Boolean) = uniffi.core.callSetMuted(muted)

    /** The default network moved; restart ICE at once. */
    fun callNetworkChanged() = uniffi.core.callNetworkChanged()

    /** One captured 20 ms frame of 48 kHz mono PCM (1920 LE bytes). */
    fun callPushAudio(pcm: ByteArray) = uniffi.core.callPushAudio(pcm)

    /** The next [frames] of playback as LE PCM; silence outside a call. */
    fun callPullAudio(frames: Int): ByteArray = uniffi.core.callPullAudio(frames.toUInt())

    /** The current call, or null when there is none. */
    fun callCurrent() = uniffi.core.callCurrent()

    /** Display name for a peer IPK — address-book, else self-asserted, else hex. */
    fun contactName(peer: ByteArray): String = uniffi.core.contactName(peer)

    /** Latest connection state, mapped to the app enum (carries @StringRes). */
    val connection: StateFlow<ConnectionState> get() = CoreEventBus.connection

    /** Live call state for the UI (null when idle). */
    val call: StateFlow<com.promtuz.core.call.CallController.Ui?>
        get() = com.promtuz.core.call.CallController.state

    /** The reactive doorbell: "these tables changed, re-read." Drives [observeQuery]. */
    val dbChanged: SharedFlow<Set<String>> get() = CoreEventBus.dbChanged

    /** Ephemeral peer typing/recording signals (not stored; UI times them out). */
    val activity: SharedFlow<ActivitySignal> get() = CoreEventBus.activity

    /** Ephemeral peer presence changes (online / last-seen). */
    val presence: SharedFlow<PresenceSignal> get() = CoreEventBus.presence

    /** Last-known presence per peer (hex IPK) — what a freshly opened chat reads first. */
    val presenceByPeer get() = CoreEventBus.presenceByPeer
}

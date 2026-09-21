package com.promtuz.chat.navigation

import androidx.navigation3.runtime.NavKey
import kotlinx.serialization.Serializable

object Routes : NavKey {
    @Serializable
    data object App : NavKey

    @Serializable
    data object Welcome : NavKey

    /** `conversation` is a hex conversation id, not a peer — a group has no peer. */
    @Serializable
    data class Chat(val conversation: String, val name: String) : NavKey

    /** A group's member list, with add / remove / leave. */
    @Serializable
    data class GroupInfo(val conversation: String) : NavKey

    @Serializable
    data object ShareIdentity : NavKey

    @Serializable
    data object Contacts : NavKey

    @Serializable
    data object Profile : NavKey

    @Serializable
    data class ProfilePhoto(val uri: String) : NavKey

    @Serializable
    data object Settings : NavKey

    @Serializable
    data object Storage : NavKey

    @Serializable
    data class StorageChat(val conversation: String, val name: String) : NavKey

    /** Onboarding: restore identity from a typed 24-word phrase. */
    @Serializable
    data object RestorePhrase : NavKey

    /** Settings: public identity actions and private recovery options. */
    @Serializable
    data object IdentityKeys : NavKey

    /** Identity & Keys: device-auth-gated reveal of the 24-word recovery phrase. */
    @Serializable
    data object RecoveryPhrase : NavKey

    @Serializable
    data object ChatAppearance : NavKey

    @Serializable
    data object About : NavKey

    @Serializable
    data object Updates : NavKey

    @Serializable
    data object OpenSourceLicenses : NavKey

    @Serializable
    data class LibraryLicense(val id: String) : NavKey

    @Serializable
    data object NotificationsSettings : NavKey

    @Serializable
    data object Logs : NavKey

    @Serializable
    data object Relays : NavKey

    /** Developer: manual snapshot / merge-restore of the encrypted backup blob. */
    @Serializable
    data object BackupRestore : NavKey

    /** Installed sticker packs, opened from the chat sticker picker. */
    @Serializable
    data object Stickers : NavKey

    /** Create a pack, or add images to [pack]. */
    @Serializable
    data class NewStickerPack(val pack: String? = null) : NavKey
}

package com.promtuz.chat.navigation

import androidx.navigation3.runtime.NavKey
import kotlinx.serialization.Serializable

object Routes : NavKey {
    @Serializable
    data object App : NavKey

    @Serializable
    data object Welcome : NavKey

    /** `conversation` is a hex conversation id, not a peer: a group has no peer. */
    @Serializable
    data class Chat(val conversation: String, val name: String) : NavKey

    @Serializable
    data class GroupInfo(val conversation: String) : NavKey

    @Serializable
    data class GroupSettings(val conversation: String) : NavKey

    @Serializable
    data object ShareIdentity : NavKey

    @Serializable
    data object Contacts : NavKey

    @Serializable
    data object Profile : NavKey

    @Serializable
    data class ProfilePhoto(val uri: String, val group: String? = null) : NavKey

    @Serializable
    data object Settings : NavKey

    @Serializable
    data class ContactCard(val peer: String = "", val name: String = "", val sharing: Boolean = false, val encoded: String? = null) : NavKey

    @Serializable
    data object MessageRequests : NavKey

    @Serializable
    data object PrivacySettings : NavKey

    @Serializable
    data class ContactInfo(val conversation: String, val peer: String) : NavKey

    @Serializable
    data class PersonInfo(val peer: String, val name: String) : NavKey

    @Serializable
    data class SharedMedia(val conversation: String, val name: String) : NavKey

    @Serializable
    data object Storage : NavKey

    @Serializable
    data class StorageChat(val conversation: String, val name: String) : NavKey

    @Serializable
    data object RestorePhrase : NavKey

    @Serializable
    data object IdentityKeys : NavKey

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

    @Serializable
    data object BackupRestore : NavKey

    @Serializable
    data object Stickers : NavKey

    /** Create a pack, or add images to [pack]. */
    @Serializable
    data class NewStickerPack(val pack: String? = null) : NavKey
}

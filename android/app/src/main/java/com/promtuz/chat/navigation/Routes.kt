package com.promtuz.chat.navigation

import androidx.navigation3.runtime.NavKey
import kotlinx.serialization.Serializable

object Routes : NavKey {
    @Serializable
    data object App : NavKey

    @Serializable
    data object Welcome : NavKey

    @Serializable
    data class Chat(val user: String, val name: String) : NavKey

    @Serializable
    data object ShareIdentity : NavKey

    @Serializable
    data object Contacts : NavKey

    @Serializable
    data object Settings : NavKey

    /** Onboarding: restore identity from a typed 24-word phrase. */
    @Serializable
    data object RestorePhrase : NavKey

    /** Settings: device-auth-gated reveal of the 24-word recovery phrase. */
    @Serializable
    data object RecoveryPhrase : NavKey

    @Serializable
    data object ChatAppearance : NavKey

    @Serializable
    data object About : NavKey

    @Serializable
    data object NotificationsSettings : NavKey

    @Serializable
    data object Logs : NavKey

    @Serializable
    data object Relays : NavKey
}
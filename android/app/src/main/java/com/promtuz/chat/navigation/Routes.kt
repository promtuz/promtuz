package com.promtuz.chat.navigation

import androidx.navigation3.runtime.NavKey
import kotlinx.serialization.Serializable

object Routes : NavKey {
    @Serializable
    data object App : NavKey

    @Serializable
    data object Chat : NavKey

    @Serializable
    data object SavedUsers : NavKey

    @Serializable
    data object Settings : NavKey

    @Serializable
    data object About : NavKey

    @Serializable
    data object Logs : NavKey

    @Serializable
    data object NetworkStats : NavKey
}
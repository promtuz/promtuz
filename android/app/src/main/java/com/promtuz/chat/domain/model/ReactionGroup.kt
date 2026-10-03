package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable

/** `mine` means the local user is among the reactors. */
@Immutable
data class ReactionGroup(val emoji: String, val count: Int, val mine: Boolean)

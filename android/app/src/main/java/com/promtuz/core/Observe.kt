package com.promtuz.core

import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.onStart

/** Reads on subscribe, then again whenever a write touches one of [tables]. The empty start tick passes the filter. */
fun <T> observeQuery(tables: Set<String>, read: suspend () -> T): Flow<T> =
    CoreEventBus.dbChanged
        .onStart { emit(emptySet()) }
        .filter { it.isEmpty() || it.any { table -> table in tables } }
        .conflate()
        .map { read() }

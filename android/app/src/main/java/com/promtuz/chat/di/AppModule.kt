package com.promtuz.chat.di

import com.promtuz.chat.update.UpdateRepository
import com.promtuz.core.CoreBridge
import org.koin.core.module.dsl.singleOf
import org.koin.dsl.module

val appModule = module {
    single { CoreBridge }
    singleOf(::UpdateRepository)
}

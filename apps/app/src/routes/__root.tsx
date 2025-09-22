import { createRootRoute, Link, Outlet } from '@tanstack/react-router'
import { TanStackRouterDevtools } from '@tanstack/react-router-devtools'

export const Route = createRootRoute({
    component: () => (
        <>
            <nav className="bg-white border-b border-gray-200 shadow-sm">
                <div className="max-w-7xl mx-auto px-6 py-4">
                    <div className="flex items-center justify-between">
                        <div className="flex items-center gap-8">
                            <h1 className="text-xl font-bold text-gray-900">Sunrise</h1>
                            <div className="flex gap-6">
                                <Link
                                    to="/"
                                    className="text-gray-600 hover:text-gray-900 transition-colors [&.active]:text-indigo-600 [&.active]:font-semibold"
                                >
                                    Home
                                </Link>
                                <Link
                                    to="/routines"
                                    className="text-gray-600 hover:text-gray-900 transition-colors [&.active]:text-indigo-600 [&.active]:font-semibold"
                                >
                                    Routines
                                </Link>
                            </div>
                        </div>
                    </div>
                </div>
            </nav>
            <main className="min-h-screen bg-gradient-to-br from-indigo-50 via-white to-purple-50">
                <Outlet />
            </main>
            <TanStackRouterDevtools />
        </>
    ),
})

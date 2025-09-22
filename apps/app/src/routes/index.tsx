import { createFileRoute } from '@tanstack/react-router'

export const Route = createFileRoute('/')({
    component: Index,
})

function Index() {
    return (
        <div className="p-6">
            <h1 className="text-3xl font-bold text-gray-900 mb-4">Welcome to Sunrise</h1>
            <p className="text-gray-600 mb-6">
                Your personal routine management and automation platform.
            </p>
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
                <div className="bg-white p-6 rounded-lg shadow-sm border border-gray-200">
                    <h3 className="text-lg font-semibold mb-2">Routine Management</h3>
                    <p className="text-gray-600">Configure and manage your daily routines with intelligent scheduling.</p>
                </div>
                <div className="bg-white p-6 rounded-lg shadow-sm border border-gray-200">
                    <h3 className="text-lg font-semibold mb-2">Smart Automation</h3>
                    <p className="text-gray-600">Let AI optimize your schedule based on priorities and preferences.</p>
                </div>
                <div className="bg-white p-6 rounded-lg shadow-sm border border-gray-200">
                    <h3 className="text-lg font-semibold mb-2">Calendar Integration</h3>
                    <p className="text-gray-600">Seamlessly sync with your existing calendar applications.</p>
                </div>
            </div>
        </div>
    )
}
// TODO: Replace this ^^

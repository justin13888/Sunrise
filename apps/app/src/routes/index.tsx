import { createFileRoute, redirect } from '@tanstack/react-router'

export const Route = createFileRoute('/')({
    beforeLoad: () => {
        // Check if user has tokens, if so redirect to schedule, otherwise to auth
        const accessToken = localStorage.getItem('access_token')

        if (accessToken) {
            throw redirect({ to: '/schedule' })
        } else {
            throw redirect({ to: '/auth' })
        }
    }
})

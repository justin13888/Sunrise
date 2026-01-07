import type {
    PriorityLevel,
    Routine,
    RoutineCategory,
    RoutineCategoryDefinition,
    TimeWindow,
} from "@sunrise/models";
import { DEFAULT_ROUTINE_CATEGORIES } from "@sunrise/models";
import { createFileRoute } from "@tanstack/react-router";
import {
    AlertCircle,
    Battery,
    BatteryLow,
    Calendar,
    CheckCircle,
    ChevronDown,
    ChevronRight,
    Clock,
    Edit2,
    Eye,
    EyeOff,
    MoreVertical,
    Plus,
    Settings,
    Trash2,
    Zap,
} from "lucide-react";
import { useMemo, useState } from "react";

export const Route = createFileRoute("/routines")({
    component: RoutinesPage,
});

// Create a lookup map for categories for efficient access
const CATEGORY_LOOKUP = new Map(
    DEFAULT_ROUTINE_CATEGORIES.map((category) => [category.id, category]),
);

// Helper function to get category definition by ID
const getCategoryById = (
    categoryId: RoutineCategory,
): RoutineCategoryDefinition | undefined => {
    return CATEGORY_LOOKUP.get(categoryId);
};

// Helper function to get category name by ID (with fallback)
const getCategoryName = (categoryId: RoutineCategory): string => {
    const category = getCategoryById(categoryId);
    return category?.name || "Unknown Category";
};

// Helper function to get category color by ID (with fallback)
const getCategoryColor = (categoryId: RoutineCategory): string => {
    const category = getCategoryById(categoryId);
    return category?.color || "#6B7280";
};

// Mock data based on our schemas
const MOCK_ROUTINES: Routine[] = [
    {
        id: "wake_up",
        name: "Wake up",
        description: "Natural wake up time with gentle transition",
        duration: {
            minutes: 15,
            flexible: true,
            min_duration: 10,
            max_duration: 30,
        },
        priority: "high",
        flexibility: 2,
        energy_level_required: "low",
        category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
        frequency: "daily",
        time_preferences: ["early_morning", "morning"],
        availability_windows: [
            { start_hour: 5, start_minute: 0, end_hour: 9, end_minute: 0 },
        ],
        dependencies: [],
        minimum_gap_minutes: 0,
        buffer_time_minutes: 0,
        conflict_resolution: "reschedule",
        can_be_grouped: false,
        enabled: true,
        tags: ["morning", "essential"],
    },
    {
        id: "morning_exercise",
        name: "Morning exercise/workout",
        description: "Energizing physical activity to start the day",
        duration: {
            minutes: 45,
            flexible: true,
            min_duration: 20,
            max_duration: 90,
        },
        priority: "high",
        flexibility: 4,
        energy_level_required: "medium",
        category: "6ba7b810-9dad-11d1-80b4-00c04fd430c8", // Health & Fitness
        frequency: "daily",
        time_preferences: ["morning", "late_morning"],
        availability_windows: [
            { start_hour: 6, start_minute: 0, end_hour: 11, end_minute: 0 },
        ],
        dependencies: [],
        minimum_gap_minutes: 30,
        buffer_time_minutes: 10,
        conflict_resolution: "reschedule",
        can_be_grouped: false,
        enabled: true,
        tags: ["morning", "fitness", "energy"],
    },
    {
        id: "deep_work_morning",
        name: "Morning deep work block",
        description: "Focused, uninterrupted work on high-priority tasks",
        duration: {
            minutes: 120,
            flexible: true,
            min_duration: 60,
            max_duration: 180,
        },
        priority: "high",
        flexibility: 3,
        energy_level_required: "high",
        category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
        frequency: "weekdays",
        time_preferences: ["morning", "late_morning"],
        availability_windows: [
            { start_hour: 8, start_minute: 0, end_hour: 12, end_minute: 0 },
        ],
        dependencies: [],
        minimum_gap_minutes: 15,
        buffer_time_minutes: 10,
        conflict_resolution: "override",
        can_be_grouped: false,
        enabled: true,
        tags: ["work", "focus", "high-priority"],
    },
    {
        id: "email_processing",
        name: "Email processing",
        description: "Dedicated time for email management and responses",
        duration: {
            minutes: 30,
            flexible: true,
            min_duration: 15,
            max_duration: 60,
        },
        priority: "medium",
        flexibility: 8,
        energy_level_required: "low",
        category: "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
        frequency: "daily",
        time_preferences: ["morning", "afternoon", "flexible"],
        availability_windows: [
            { start_hour: 9, start_minute: 0, end_hour: 11, end_minute: 0 },
            { start_hour: 14, start_minute: 0, end_hour: 16, end_minute: 0 },
        ],
        dependencies: [],
        minimum_gap_minutes: 0,
        buffer_time_minutes: 5,
        conflict_resolution: "compress",
        can_be_grouped: true,
        preferred_batch_size: 2,
        enabled: true,
        tags: ["work", "communication", "admin"],
    },
    {
        id: "wind_down",
        name: "Wind down routine",
        description: "Relaxing activities to prepare for sleep",
        duration: {
            minutes: 45,
            flexible: true,
            min_duration: 30,
            max_duration: 90,
        },
        priority: "high",
        flexibility: 5,
        energy_level_required: "low",
        category: "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
        frequency: "daily",
        time_preferences: ["evening", "night"],
        availability_windows: [
            { start_hour: 20, start_minute: 0, end_hour: 23, end_minute: 0 },
        ],
        dependencies: [],
        minimum_gap_minutes: 0,
        buffer_time_minutes: 10,
        conflict_resolution: "compress",
        can_be_grouped: false,
        enabled: false,
        tags: ["evening", "relaxation", "sleep-prep"],
    },
];

function RoutinesPage() {
    const [routines, setRoutines] = useState(MOCK_ROUTINES);
    const [selectedRoutineId, setSelectedRoutineId] = useState<string | null>(
        null,
    );
    const selectedRoutine = useMemo(() => {
        return routines.find((r) => r.id === selectedRoutineId) || null;
    }, [selectedRoutineId, routines]);
    const [editMode, setEditMode] = useState(false);
    const [expandedCategories, setExpandedCategories] = useState(
        new Set([
            "f47ac10b-58cc-4372-a567-0e02b2c3d479", // Personal Care
            "550e8400-e29b-41d4-a716-446655440000", // Work & Professional
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8", // Health & Fitness
        ]),
    );
    const [viewMode, setViewMode] = useState("list"); // 'list' or 'timeline'

    // Group routines by category
    const routinesByCategory: Map<RoutineCategory, Routine[]> = useMemo(() => {
        return routines.reduce((acc, routine) => {
            const key = routine.category;
            let array = acc.get(key);
            if (!array) {
                array = [];
                acc.set(key, array);
            }
            array.push(routine);

            return acc;
        }, new Map<RoutineCategory, Routine[]>());
    }, [routines]);

    // Get priority color
    const getPriorityColor = (priority: PriorityLevel) => {
        switch (priority) {
            case "high":
                return "text-red-600 bg-red-50 border-red-200";
            case "medium":
                return "text-yellow-600 bg-yellow-50 border-yellow-200";
            case "low":
                return "text-green-600 bg-green-50 border-green-200";
            default:
                return "text-gray-600 bg-gray-50 border-gray-200";
        }
    };

    // Get energy level icon
    const getEnergyIcon = (level: PriorityLevel) => {
        switch (level) {
            case "high":
                return <Zap className="w-4 h-4 text-red-500" />;
            case "medium":
                return <Battery className="w-4 h-4 text-yellow-500" />;
            case "low":
                return <BatteryLow className="w-4 h-4 text-green-500" />;
            default:
                return null;
        }
    };

    // Format time window
    const formatTimeWindow = (window: TimeWindow) => {
        const formatTime = (hour: number, minute = 0) => {
            const period = hour >= 12 ? "PM" : "AM";
            const displayHour = hour === 0 ? 12 : hour > 12 ? hour - 12 : hour;
            return `${displayHour}${minute ? `:${minute.toString().padStart(2, "0")}` : ""}${period}`;
        };
        return `${formatTime(window.start_hour, window.start_minute)} - ${formatTime(window.end_hour, window.end_minute)}`;
    };

    // Toggle category expansion
    const toggleCategory = (category: RoutineCategory) => {
        const newExpanded = new Set(expandedCategories);
        if (newExpanded.has(category)) {
            newExpanded.delete(category);
        } else {
            newExpanded.add(category);
        }
        setExpandedCategories(newExpanded);
    };

    // Toggle routine enabled status
    const toggleRoutineEnabled = (routineId: string) => {
        setRoutines((prev) =>
            prev.map((r) =>
                r.id === routineId ? { ...r, enabled: !r.enabled } : r,
            ),
        );
    };

    // Delete routine
    const deleteRoutine = (routineId: string) => {
        setRoutines((prev) => prev.filter((r) => r.id !== routineId));
        if (selectedRoutineId === routineId) {
            setSelectedRoutineId(null);
            setEditMode(false);
        }
    };

    // Stats calculation
    const stats = useMemo(() => {
        const enabled = routines.filter((r) => r.enabled);
        const totalTime = enabled.reduce(
            (sum, r) => sum + r.duration.minutes,
            0,
        );
        const highPriority = enabled.filter(
            (r) => r.priority === "high",
        ).length;
        const categories = new Set(enabled.map((r) => r.category)).size;

        return {
            totalRoutines: enabled.length,
            totalTime: Math.round((totalTime / 60) * 10) / 10, // hours
            highPriority,
            categories,
        };
    }, [routines]);

    return (
        <div className="max-w-7xl mx-auto p-6">
            {/* Header */}
            <div className="mb-8">
                <div className="flex items-center justify-between mb-4">
                    <div>
                        <h1 className="text-3xl font-bold text-gray-900 mb-2">
                            Routine Configuration
                        </h1>
                        <p className="text-gray-600">
                            Manage your daily routines and automation
                            preferences
                        </p>
                    </div>
                    <div className="flex items-center gap-3">
                        <button
                            type="button"
                            onClick={() =>
                                setViewMode(
                                    viewMode === "list" ? "timeline" : "list",
                                )
                            }
                            className="px-4 py-2 bg-white border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors flex items-center gap-2"
                        >
                            <Calendar className="w-4 h-4" />
                            {viewMode === "list"
                                ? "Timeline View"
                                : "List View"}
                        </button>
                        <button
                            type="button"
                            className="px-4 py-2 bg-indigo-600 text-white rounded-lg hover:bg-indigo-700 transition-colors flex items-center gap-2"
                        >
                            <Plus className="w-4 h-4" />
                            Add Routine
                        </button>
                    </div>
                </div>

                {/* Stats Overview */}
                <div className="grid grid-cols-4 gap-4 mb-6">
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Active Routines
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.totalRoutines}
                                </p>
                            </div>
                            <CheckCircle className="w-8 h-8 text-green-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Daily Time
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.totalTime}h
                                </p>
                            </div>
                            <Clock className="w-8 h-8 text-blue-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    High Priority
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.highPriority}
                                </p>
                            </div>
                            <AlertCircle className="w-8 h-8 text-red-500" />
                        </div>
                    </div>
                    <div className="bg-white rounded-xl p-4 border border-gray-200 shadow-sm">
                        <div className="flex items-center justify-between">
                            <div>
                                <p className="text-sm text-gray-600">
                                    Categories
                                </p>
                                <p className="text-2xl font-bold text-gray-900">
                                    {stats.categories}
                                </p>
                            </div>
                            <Settings className="w-8 h-8 text-purple-500" />
                        </div>
                    </div>
                </div>
            </div>

            <div className="grid grid-cols-3 gap-6">
                {/* Routines List */}
                <div className="col-span-2 space-y-4">
                    {Array.from(routinesByCategory.entries()).map(
                        ([categoryId, categoryRoutines]: [
                            string,
                            Routine[],
                        ]) => {
                            const categoryName = getCategoryName(categoryId);
                            const categoryColor = getCategoryColor(categoryId);

                            return (
                                <div
                                    key={categoryId}
                                    className="bg-white rounded-xl border border-gray-200 shadow-sm overflow-hidden"
                                >
                                    <button
                                        type="button"
                                        onClick={() =>
                                            toggleCategory(
                                                categoryId as RoutineCategory,
                                            )
                                        }
                                        className="w-full px-6 py-4 flex items-center justify-between bg-gray-50 hover:bg-gray-100 transition-colors"
                                    >
                                        <div className="flex items-center gap-3 w-full">
                                            {expandedCategories.has(
                                                categoryId,
                                            ) ? (
                                                <ChevronDown className="w-5 h-5 text-gray-400" />
                                            ) : (
                                                <ChevronRight className="w-5 h-5 text-gray-400" />
                                            )}
                                            <div
                                                className="w-3 h-3 rounded-full"
                                                style={{
                                                    backgroundColor:
                                                        categoryColor,
                                                }}
                                            />
                                            <h3 className="font-semibold text-gray-900">
                                                {categoryName}
                                            </h3>
                                            <span className="px-2 py-1 bg-gray-200 text-gray-700 text-sm rounded-full">
                                                {
                                                    categoryRoutines.filter(
                                                        (r) => r.enabled,
                                                    ).length
                                                }
                                                /{categoryRoutines.length}
                                            </span>
                                            <div className="ml-auto relative">
                                                <button
                                                    type="button"
                                                    onClick={(e) => {
                                                        e.stopPropagation();
                                                        // TODO: Handle category options menu
                                                    }}
                                                    className="p-1 hover:bg-gray-200 rounded"
                                                >
                                                    <MoreVertical className="w-4 h-4 text-gray-400" />
                                                </button>
                                            </div>
                                        </div>
                                    </button>

                                    {expandedCategories.has(categoryId) && (
                                        <div className="divide-y divide-gray-100">
                                            {categoryRoutines.map((routine) => (
                                                <button
                                                    key={routine.id}
                                                    type="button"
                                                    className={`w-full text-left p-4 hover:bg-gray-50 transition-colors cursor-pointer ${
                                                        selectedRoutineId ===
                                                        routine.id
                                                            ? "bg-indigo-50 border-l-4 border-indigo-500"
                                                            : ""
                                                    } ${!routine.enabled ? "opacity-60" : ""}`}
                                                    onClick={() =>
                                                        setSelectedRoutineId(
                                                            routine.id,
                                                        )
                                                    }
                                                >
                                                    <div className="flex items-center justify-between">
                                                        <div className="flex-1">
                                                            <div className="flex items-center gap-3 mb-2">
                                                                <h4 className="font-medium text-gray-900">
                                                                    {
                                                                        routine.name
                                                                    }
                                                                </h4>
                                                                <span
                                                                    className={`px-2 py-1 text-xs rounded-full border ${getPriorityColor(routine.priority)}`}
                                                                >
                                                                    {
                                                                        routine.priority
                                                                    }
                                                                </span>
                                                                {getEnergyIcon(
                                                                    routine.energy_level_required,
                                                                )}
                                                            </div>
                                                            <p className="text-sm text-gray-600 mb-2">
                                                                {
                                                                    routine.description
                                                                }
                                                            </p>
                                                            <div className="flex items-center gap-4 text-xs text-gray-500">
                                                                <span className="flex items-center gap-1">
                                                                    <Clock className="w-3 h-3" />
                                                                    {routine
                                                                        .duration
                                                                        .flexible
                                                                        ? `${routine.duration.min_duration}-${routine.duration.max_duration}min`
                                                                        : `${routine.duration.minutes}min`}
                                                                </span>
                                                                <span>
                                                                    {
                                                                        routine.frequency
                                                                    }
                                                                </span>
                                                                <span>
                                                                    Flexibility:{" "}
                                                                    {
                                                                        routine.flexibility
                                                                    }
                                                                    /10
                                                                </span>
                                                            </div>
                                                        </div>
                                                        <div className="flex items-center gap-2 ml-4">
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    toggleRoutineEnabled(
                                                                        routine.id,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                {routine.enabled ? (
                                                                    <Eye className="w-4 h-4 text-green-600" />
                                                                ) : (
                                                                    <EyeOff className="w-4 h-4 text-gray-400" />
                                                                )}
                                                            </button>
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    setSelectedRoutineId(
                                                                        routine.id,
                                                                    );
                                                                    setEditMode(
                                                                        true,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                <Edit2 className="w-4 h-4 text-blue-600" />
                                                            </button>
                                                            <button
                                                                type="button"
                                                                onClick={(
                                                                    e,
                                                                ) => {
                                                                    e.stopPropagation();
                                                                    deleteRoutine(
                                                                        routine.id,
                                                                    );
                                                                }}
                                                                className="p-1 hover:bg-gray-200 rounded"
                                                            >
                                                                <Trash2 className="w-4 h-4 text-red-600" />
                                                            </button>
                                                        </div>
                                                    </div>
                                                </button>
                                            ))}
                                        </div>
                                    )}
                                </div>
                            );
                        },
                    )}
                </div>

                {/* Detail Panel */}
                <div className="bg-white rounded-xl border border-gray-200 shadow-sm">
                    {selectedRoutine ? (
                        <div className="p-6">
                            <div className="flex items-center justify-between mb-4">
                                <h3 className="text-lg font-semibold text-gray-900">
                                    {editMode
                                        ? "Edit Routine"
                                        : "Routine Details"}
                                </h3>
                                <div className="flex items-center gap-2">
                                    {editMode ? (
                                        <>
                                            <button
                                                type="button"
                                                onClick={() =>
                                                    setEditMode(false)
                                                }
                                                className="px-3 py-1 text-sm bg-gray-100 text-gray-700 rounded hover:bg-gray-200"
                                            >
                                                Cancel
                                            </button>
                                            <button
                                                type="button"
                                                className="px-3 py-1 text-sm bg-indigo-600 text-white rounded hover:bg-indigo-700"
                                            >
                                                Save
                                            </button>
                                        </>
                                    ) : (
                                        <button
                                            type="button"
                                            onClick={() => setEditMode(true)}
                                            className="p-2 hover:bg-gray-100 rounded"
                                        >
                                            <Edit2 className="w-4 h-4" />
                                        </button>
                                    )}
                                </div>
                            </div>

                            {editMode ? (
                                <div className="space-y-4">
                                    <div>
                                        <label
                                            htmlFor="editName"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Name
                                        </label>
                                        <input
                                            id="editName"
                                            type="text"
                                            value={selectedRoutine.name}
                                            className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                        />
                                    </div>
                                    <div>
                                        <label
                                            htmlFor="description"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Description
                                        </label>
                                        <textarea
                                            id="description"
                                            value={selectedRoutine.description}
                                            rows={3}
                                            className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                        />
                                    </div>
                                    <div className="grid grid-cols-2 gap-4">
                                        <div>
                                            <label
                                                htmlFor="duration"
                                                className="block text-sm font-medium text-gray-700 mb-1"
                                            >
                                                Duration (min)
                                            </label>
                                            <input
                                                id="duration"
                                                type="number"
                                                value={
                                                    selectedRoutine.duration
                                                        .minutes
                                                }
                                                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                            />
                                        </div>
                                        <div>
                                            <label
                                                htmlFor="priority"
                                                className="block text-sm font-medium text-gray-700 mb-1"
                                            >
                                                Priority
                                            </label>
                                            <select
                                                id="priority"
                                                value={selectedRoutine.priority}
                                                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-indigo-500 focus:border-indigo-500"
                                            >
                                                <option value="low">Low</option>
                                                <option value="medium">
                                                    Medium
                                                </option>
                                                <option value="high">
                                                    High
                                                </option>
                                            </select>
                                        </div>
                                    </div>
                                    <div>
                                        <label
                                            htmlFor="flexibility"
                                            className="block text-sm font-medium text-gray-700 mb-1"
                                        >
                                            Flexibility (
                                            {selectedRoutine.flexibility}/10)
                                        </label>
                                        <input
                                            id="flexibility"
                                            type="range"
                                            min="0"
                                            max="10"
                                            value={selectedRoutine.flexibility}
                                            className="w-full"
                                        />
                                        <div className="flex justify-between text-xs text-gray-500 mt-1">
                                            <span>Rigid</span>
                                            <span>Very Flexible</span>
                                        </div>
                                    </div>
                                </div>
                            ) : (
                                <div className="space-y-6">
                                    <div>
                                        <h4 className="font-medium text-gray-900 mb-2">
                                            {selectedRoutine.name}
                                        </h4>
                                        <p className="text-sm text-gray-600 mb-4">
                                            {selectedRoutine.description}
                                        </p>

                                        <div className="grid grid-cols-2 gap-4 text-sm">
                                            <div>
                                                <span className="text-gray-500">
                                                    Duration:
                                                </span>
                                                <span className="ml-2 font-medium">
                                                    {selectedRoutine.duration
                                                        .flexible
                                                        ? `${selectedRoutine.duration.min_duration}-${selectedRoutine.duration.max_duration} min`
                                                        : `${selectedRoutine.duration.minutes} min`}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Priority:
                                                </span>
                                                <span
                                                    className={`ml-2 px-2 py-1 text-xs rounded ${getPriorityColor(selectedRoutine.priority)}`}
                                                >
                                                    {selectedRoutine.priority}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Energy:
                                                </span>
                                                <span className="ml-2 flex items-center gap-1">
                                                    {getEnergyIcon(
                                                        selectedRoutine.energy_level_required,
                                                    )}
                                                    {
                                                        selectedRoutine.energy_level_required
                                                    }
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Frequency:
                                                </span>
                                                <span className="ml-2 font-medium">
                                                    {selectedRoutine.frequency}
                                                </span>
                                            </div>
                                            <div>
                                                <span className="text-gray-500">
                                                    Category:
                                                </span>
                                                <span className="ml-2 flex items-center gap-2">
                                                    <div
                                                        className="w-3 h-3 rounded-full"
                                                        style={{
                                                            backgroundColor:
                                                                getCategoryColor(
                                                                    selectedRoutine.category,
                                                                ),
                                                        }}
                                                    />
                                                    {getCategoryName(
                                                        selectedRoutine.category,
                                                    )}
                                                </span>
                                            </div>
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Time Preferences
                                        </h5>
                                        <div className="flex flex-wrap gap-2">
                                            {selectedRoutine.time_preferences.map(
                                                (time) => (
                                                    <span
                                                        key={time}
                                                        className="px-2 py-1 bg-blue-100 text-blue-800 text-xs rounded-full"
                                                    >
                                                        {time.replace("_", " ")}
                                                    </span>
                                                ),
                                            )}
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Availability Windows
                                        </h5>
                                        <div className="space-y-1">
                                            {selectedRoutine.availability_windows.map(
                                                (window, index) => (
                                                    <div
                                                        key={`window-${selectedRoutine.id}-${index}`}
                                                        className="text-sm text-gray-600"
                                                    >
                                                        {formatTimeWindow(
                                                            window,
                                                        )}
                                                    </div>
                                                ),
                                            )}
                                        </div>
                                    </div>

                                    <div>
                                        <h5 className="font-medium text-gray-900 mb-2">
                                            Settings
                                        </h5>
                                        <div className="text-sm space-y-1">
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Flexibility:
                                                </span>
                                                <span>
                                                    {
                                                        selectedRoutine.flexibility
                                                    }
                                                    /10
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Buffer Time:
                                                </span>
                                                <span>
                                                    {
                                                        selectedRoutine.buffer_time_minutes
                                                    }{" "}
                                                    min
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Conflict Resolution:
                                                </span>
                                                <span className="capitalize">
                                                    {
                                                        selectedRoutine.conflict_resolution
                                                    }
                                                </span>
                                            </div>
                                            <div className="flex justify-between">
                                                <span className="text-gray-500">
                                                    Can Group:
                                                </span>
                                                <span>
                                                    {selectedRoutine.can_be_grouped
                                                        ? "Yes"
                                                        : "No"}
                                                </span>
                                            </div>
                                        </div>
                                    </div>

                                    {selectedRoutine.tags.length > 0 && (
                                        <div>
                                            <h5 className="font-medium text-gray-900 mb-2">
                                                Tags
                                            </h5>
                                            <div className="flex flex-wrap gap-1">
                                                {selectedRoutine.tags.map(
                                                    (tag) => (
                                                        <span
                                                            key={tag}
                                                            className="px-2 py-1 bg-gray-100 text-gray-700 text-xs rounded"
                                                        >
                                                            {tag}
                                                        </span>
                                                    ),
                                                )}
                                            </div>
                                        </div>
                                    )}
                                </div>
                            )}
                        </div>
                    ) : (
                        <div className="p-6 text-center text-gray-500">
                            <Calendar className="w-12 h-12 mx-auto mb-4 text-gray-300" />
                            <p>Select a routine to view details</p>
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
}

// TODO: Finish this ^^
